//! 可选进程级流量消费者。
//!
//! user daemon 只读取 system D-Bus helper 的聚合结果，不接收每包事件；默认
//! `backend=off` 时不连接任何总线。特权 helper（eBPF/AF_PACKET）由单独
//! systemd 单元提供，本模块在 helper 缺失或能力不足时明确降级并给出原因。

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use tokio::{sync::watch, task::JoinHandle, time::sleep};
use tracing::{debug, warn};
use trafficview_core::{config::ProcessTrafficBackend, ProcessTrafficSample};
use zbus::zvariant::OwnedValue;

pub const HELPER_BUS_NAME: &str = "com.trafficview.Ebpf1";
pub const HELPER_PATH: &str = "/com/trafficview/Ebpf1";
pub const HELPER_INTERFACE: &str = "com.trafficview.Ebpf1";
/// 单次 helper 查询上限；超时不阻塞下一轮采样。
pub const HELPER_QUERY_TIMEOUT: Duration = Duration::from_secs(1);
/// Snapshot 中保留的进程条数；helper 负责 UID 过滤。
pub const TOP_PROCESSES: usize = 10;

const DBUS_NAME: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const DBUS_INTERFACE: &str = "org.freedesktop.DBus";

/// `GetBackendStatus` 的解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperStatus {
    pub available: bool,
    pub backend: String,
    pub reason: String,
    pub capabilities: Vec<String>,
}

/// 暴露给调度器的进程流量可用性。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcessTrafficStatus {
    pub available: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
struct Cache {
    processes: Option<Vec<ProcessTrafficSample>>,
    status: ProcessTrafficStatus,
}

/// 后台消费者句柄；查询在独立 tokio 任务中进行，不阻塞内置采集。
pub struct ProcessTrafficHandle {
    cache: Arc<Mutex<Cache>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl ProcessTrafficHandle {
    pub fn processes(&self) -> Option<Vec<ProcessTrafficSample>> {
        lock(&self.cache).processes.clone()
    }

    pub fn status(&self) -> ProcessTrafficStatus {
        lock(&self.cache).status.clone()
    }

    /// 停止后台任务；可重复调用。
    pub fn stop(&self) {
        if let Some(task) = lock(&self.task).take() {
            task.abort();
        }
    }
}

impl Drop for ProcessTrafficHandle {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.task.lock() {
            if let Some(task) = slot.take() {
                task.abort();
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 启动消费者；`Off` 只发布原因，不连接 system bus。
pub fn spawn(
    backend: ProcessTrafficBackend,
    interval: Duration,
    shutdown: watch::Receiver<bool>,
) -> ProcessTrafficHandle {
    let reason = if backend == ProcessTrafficBackend::Off {
        "process traffic backend is off"
    } else {
        "waiting for the process traffic helper"
    };
    let cache = Arc::new(Mutex::new(Cache {
        processes: None,
        status: ProcessTrafficStatus {
            available: false,
            reason: reason.into(),
        },
    }));
    let task_cache = cache.clone();
    let task = tokio::spawn(run(task_cache, backend, interval, shutdown));
    ProcessTrafficHandle {
        cache,
        task: Mutex::new(Some(task)),
    }
}

async fn run(
    cache: Arc<Mutex<Cache>>,
    backend: ProcessTrafficBackend,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    if backend == ProcessTrafficBackend::Off {
        return;
    }
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            _ = sleep(interval) => {}
        }
        match tokio::time::timeout(HELPER_QUERY_TIMEOUT, query(backend)).await {
            Ok(Ok(processes)) => {
                debug!(count = processes.len(), "process traffic updated");
                let mut cache = lock(&cache);
                cache.processes = Some(processes);
                cache.status = ProcessTrafficStatus {
                    available: true,
                    reason: String::new(),
                };
            }
            Ok(Err(reason)) => {
                warn!(%reason, "process traffic helper unavailable");
                publish_error(&cache, reason);
            }
            Err(_) => {
                let reason = format!(
                    "process traffic helper did not answer within {}ms",
                    HELPER_QUERY_TIMEOUT.as_millis()
                );
                warn!(%reason, "process traffic helper timed out");
                publish_error(&cache, reason);
            }
        }
    }
}

fn publish_error(cache: &Mutex<Cache>, reason: String) {
    let mut cache = lock(cache);
    // 失败时不保留旧值，避免把过期排行当作新采样。
    cache.processes = None;
    cache.status = ProcessTrafficStatus {
        available: false,
        reason,
    };
}

async fn query(requested: ProcessTrafficBackend) -> Result<Vec<ProcessTrafficSample>, String> {
    let connection = zbus::Connection::system()
        .await
        .map_err(|error| format!("system D-Bus is unavailable: {error}"))?;
    let owned: bool = connection
        .call_method(
            Some(DBUS_NAME),
            DBUS_PATH,
            Some(DBUS_INTERFACE),
            "NameHasOwner",
            &(HELPER_BUS_NAME,),
        )
        .await
        .map_err(|error| format!("cannot query system D-Bus: {error}"))?
        .body()
        .deserialize()
        .map_err(|error| format!("invalid NameHasOwner reply: {error}"))?;
    if !owned {
        return Err(helper_missing_reason());
    }
    let status_reply = connection
        .call_method(
            Some(HELPER_BUS_NAME),
            HELPER_PATH,
            Some(HELPER_INTERFACE),
            "GetBackendStatus",
            &(),
        )
        .await
        .map_err(|error| format!("GetBackendStatus failed: {error}"))?;
    let status: HashMap<String, OwnedValue> = status_reply
        .body()
        .deserialize()
        .map_err(|error| format!("invalid GetBackendStatus reply: {error}"))?;
    let helper = parse_helper_status(&status)?;
    resolve_backend(requested, Some(&helper))?;

    let reply = connection
        .call_method(
            Some(HELPER_BUS_NAME),
            HELPER_PATH,
            Some(HELPER_INTERFACE),
            "GetProcesses",
            &(),
        )
        .await
        .map_err(|error| format!("GetProcesses failed: {error}"))?;
    let processes: HashMap<String, OwnedValue> = reply
        .body()
        .deserialize()
        .map_err(|error| format!("invalid GetProcesses reply: {error}"))?;
    parse_processes(&processes)
}

fn helper_missing_reason() -> String {
    format!("{HELPER_BUS_NAME} is not running; install and enable trafficviewd-ebpf.service")
}

/// 解析 helper 能力；缺少可选字段时按空处理。
pub fn parse_helper_status(dict: &HashMap<String, OwnedValue>) -> Result<HelperStatus, String> {
    let available = bool::try_from(field(dict, "available", "GetBackendStatus")?)
        .map_err(|error| format!("GetBackendStatus.available: {error}"))?;
    let backend = String::try_from(field(dict, "backend", "GetBackendStatus")?)
        .map_err(|error| format!("GetBackendStatus.backend: {error}"))?;
    let reason = match dict.get("reason") {
        Some(value) => String::try_from(
            value
                .try_clone()
                .map_err(|error| format!("GetBackendStatus.reason: {error}"))?,
        )
        .map_err(|error| format!("GetBackendStatus.reason: {error}"))?,
        None => String::new(),
    };
    let capabilities = match dict.get("capabilities") {
        Some(value) => Vec::<String>::try_from(
            value
                .try_clone()
                .map_err(|error| format!("GetBackendStatus.capabilities: {error}"))?,
        )
        .map_err(|error| format!("GetBackendStatus.capabilities: {error}"))?,
        None => Vec::new(),
    };
    Ok(HelperStatus {
        available,
        backend,
        reason,
        capabilities,
    })
}

/// 按配置选择实际后端；`Off` 永不查询，其余情况缺失能力时返回安装提示。
pub fn resolve_backend(
    requested: ProcessTrafficBackend,
    status: Option<&HelperStatus>,
) -> Result<Option<&'static str>, String> {
    if requested == ProcessTrafficBackend::Off {
        return Ok(None);
    }
    let Some(status) = status else {
        return Err(helper_missing_reason());
    };
    if !status.available {
        return Err(if status.reason.trim().is_empty() {
            format!(
                "process traffic helper backend '{}' is unavailable",
                status.backend
            )
        } else {
            status.reason.clone()
        });
    }
    let supports = |name: &str| status.capabilities.iter().any(|cap| cap == name);
    let selected = match requested {
        ProcessTrafficBackend::Off => unreachable!("off handled above"),
        ProcessTrafficBackend::Ebpf => "ebpf",
        ProcessTrafficBackend::AfPacket => "af_packet",
        ProcessTrafficBackend::Auto => {
            if supports("ebpf") {
                "ebpf"
            } else if supports("af_packet") {
                "af_packet"
            } else {
                return Err(format!(
                    "helper capabilities {:?} include neither ebpf nor af_packet",
                    status.capabilities
                ));
            }
        }
    };
    if !supports(selected) {
        return Err(format!(
            "helper capabilities {:?} do not include {selected}",
            status.capabilities
        ));
    }
    Ok(Some(selected))
}

/// 解析 `GetProcesses` 的 `processes` 数组，按速率降序取 Top N。
pub fn parse_processes(
    dict: &HashMap<String, OwnedValue>,
) -> Result<Vec<ProcessTrafficSample>, String> {
    let value = dict
        .get("processes")
        .ok_or_else(|| "GetProcesses is missing 'processes'".to_string())?;
    let rows = Vec::<OwnedValue>::try_from(
        value
            .try_clone()
            .map_err(|error| format!("GetProcesses.processes: {error}"))?,
    )
    .map_err(|error| format!("GetProcesses.processes: expected av: {error}"))?;
    let mut samples = Vec::with_capacity(rows.len());
    for (index, row) in rows.into_iter().enumerate() {
        let path = format!("processes[{index}]");
        let row: HashMap<String, OwnedValue> = row
            .try_into()
            .map_err(|error| format!("{path}: expected a{{sv}}: {error}"))?;
        let pid = u32::try_from(field(&row, "pid", &path)?)
            .map_err(|error| format!("{path}.pid: {error}"))?;
        if pid == 0 {
            return Err(format!("{path}.pid must not be zero"));
        }
        samples.push(ProcessTrafficSample {
            pid,
            name: String::try_from(field(&row, "name", &path)?)
                .map_err(|error| format!("{path}.name: {error}"))?,
            rx_bps: u64::try_from(field(&row, "rx_bps", &path)?)
                .map_err(|error| format!("{path}.rx_bps: {error}"))?,
            tx_bps: u64::try_from(field(&row, "tx_bps", &path)?)
                .map_err(|error| format!("{path}.tx_bps: {error}"))?,
            rx_total: u64::try_from(field(&row, "rx_total", &path)?)
                .map_err(|error| format!("{path}.rx_total: {error}"))?,
            tx_total: u64::try_from(field(&row, "tx_total", &path)?)
                .map_err(|error| format!("{path}.tx_total: {error}"))?,
        });
    }
    samples.sort_by(|left, right| {
        let left_total = u128::from(left.rx_bps) + u128::from(left.tx_bps);
        let right_total = u128::from(right.rx_bps) + u128::from(right.tx_bps);
        right_total
            .cmp(&left_total)
            .then_with(|| left.pid.cmp(&right.pid))
    });
    samples.truncate(TOP_PROCESSES);
    Ok(samples)
}

fn field(
    dict: &HashMap<String, OwnedValue>,
    key: &str,
    context: &str,
) -> Result<OwnedValue, String> {
    dict.get(key)
        .ok_or_else(|| format!("{context} is missing '{key}'"))?
        .try_clone()
        .map_err(|error| format!("{context}.{key}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    fn helper(capabilities: &[&str]) -> HelperStatus {
        HelperStatus {
            available: true,
            backend: "af_packet".into(),
            reason: String::new(),
            capabilities: capabilities.iter().map(|cap| (*cap).to_owned()).collect(),
        }
    }

    fn process_value(pid: u32, name: &str, rx_bps: u64, tx_bps: u64) -> Value<'static> {
        let mut map: HashMap<String, Value<'static>> = HashMap::new();
        map.insert("pid".into(), Value::from(pid));
        map.insert("name".into(), Value::from(name.to_owned()));
        map.insert("rx_bps".into(), Value::from(rx_bps));
        map.insert("tx_bps".into(), Value::from(tx_bps));
        map.insert("rx_total".into(), Value::from(rx_bps * 10));
        map.insert("tx_total".into(), Value::from(tx_bps * 10));
        Value::from(map)
    }

    fn process_dict(rows: Vec<Value<'static>>) -> HashMap<String, OwnedValue> {
        let mut dict: HashMap<String, OwnedValue> = HashMap::new();
        dict.insert(
            "processes".into(),
            OwnedValue::try_from(Value::from(rows)).unwrap(),
        );
        dict
    }

    #[test]
    fn off_backend_never_queries_the_helper() {
        assert_eq!(
            resolve_backend(ProcessTrafficBackend::Off, None).unwrap(),
            None
        );
    }

    #[test]
    fn auto_prefers_ebpf_then_af_packet() {
        assert_eq!(
            resolve_backend(
                ProcessTrafficBackend::Auto,
                Some(&helper(&["af_packet", "ebpf"]))
            )
            .unwrap(),
            Some("ebpf")
        );
        assert_eq!(
            resolve_backend(ProcessTrafficBackend::Auto, Some(&helper(&["af_packet"]))).unwrap(),
            Some("af_packet")
        );
        let error = resolve_backend(ProcessTrafficBackend::Auto, Some(&helper(&[]))).unwrap_err();
        assert!(error.contains("neither ebpf nor af_packet"), "{error}");
    }

    #[test]
    fn explicit_backend_requires_matching_capability() {
        let error = resolve_backend(ProcessTrafficBackend::Ebpf, Some(&helper(&["af_packet"])))
            .unwrap_err();
        assert!(error.contains("do not include ebpf"), "{error}");
        let error =
            resolve_backend(ProcessTrafficBackend::AfPacket, Some(&helper(&["ebpf"]))).unwrap_err();
        assert!(error.contains("do not include af_packet"), "{error}");
        assert_eq!(
            resolve_backend(
                ProcessTrafficBackend::Ebpf,
                Some(&helper(&["ebpf", "af_packet"]))
            )
            .unwrap(),
            Some("ebpf")
        );
    }

    #[test]
    fn missing_helper_reports_the_system_service_hint() {
        let error = resolve_backend(ProcessTrafficBackend::Ebpf, None).unwrap_err();
        assert!(error.contains("trafficviewd-ebpf.service"), "{error}");
    }

    #[test]
    fn unavailable_helper_keeps_its_reason() {
        let status = HelperStatus {
            available: false,
            backend: "ebpf".into(),
            reason: "missing CAP_BPF".into(),
            capabilities: vec!["ebpf".into()],
        };
        let error = resolve_backend(ProcessTrafficBackend::Auto, Some(&status)).unwrap_err();
        assert_eq!(error, "missing CAP_BPF");
    }

    #[test]
    fn process_rows_are_sorted_and_truncated() {
        let rows = (1..=12)
            .map(|pid| process_value(pid, &format!("proc{pid}"), u64::from(pid), 0))
            .collect();
        let samples = parse_processes(&process_dict(rows)).unwrap();
        assert_eq!(samples.len(), TOP_PROCESSES);
        assert_eq!(samples[0].pid, 12);
        assert_eq!(samples[0].rx_bps, 12);
        assert_eq!(samples[9].pid, 3);
    }

    #[test]
    fn invalid_process_rows_are_rejected() {
        let mut missing_name: HashMap<String, Value<'static>> = HashMap::new();
        missing_name.insert("pid".into(), Value::from(7u32));
        missing_name.insert("rx_bps".into(), Value::from(1u64));
        missing_name.insert("tx_bps".into(), Value::from(1u64));
        missing_name.insert("rx_total".into(), Value::from(1u64));
        missing_name.insert("tx_total".into(), Value::from(1u64));
        let dict = process_dict(vec![Value::from(missing_name)]);
        assert!(parse_processes(&dict).unwrap_err().contains("name"));

        let zero_pid = vec![process_value(0, "zero", 1, 1)];
        assert!(parse_processes(&process_dict(zero_pid))
            .unwrap_err()
            .contains("must not be zero"));
    }

    #[test]
    fn helper_status_parses_optional_fields() {
        let mut dict: HashMap<String, OwnedValue> = HashMap::new();
        dict.insert(
            "available".into(),
            OwnedValue::try_from(Value::from(true)).unwrap(),
        );
        dict.insert(
            "backend".into(),
            OwnedValue::try_from(Value::from("ebpf")).unwrap(),
        );
        dict.insert(
            "capabilities".into(),
            OwnedValue::try_from(Value::from(vec!["ebpf".to_string()])).unwrap(),
        );
        let status = parse_helper_status(&dict).unwrap();
        assert!(status.available);
        assert_eq!(status.backend, "ebpf");
        assert_eq!(status.capabilities, ["ebpf"]);
        assert!(status.reason.is_empty());
    }

    #[tokio::test]
    async fn off_backend_publishes_reason_without_touching_the_bus() {
        let (_tx, rx) = watch::channel(false);
        let handle = spawn(ProcessTrafficBackend::Off, Duration::from_secs(1), rx);
        assert!(handle.processes().is_none());
        let status = handle.status();
        assert!(!status.available);
        assert!(status.reason.contains("off"), "{status:?}");
        handle.stop();
    }
}
