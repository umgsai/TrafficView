//! 特权进程流量 helper：AF_PACKET 抓包 + system D-Bus 只读接口。
//!
//! 本版本只内置 AF_PACKET；`--backend ebpf` 会明确报告未构建。服务必须拥有
//! `com.trafficview.Ebpf1`，并按调用者 UID 过滤结果；root 可取全部用户。

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

use anyhow::Context;
use trafficview_core::ProcessTrafficSample;
use trafficview_daemon::netprobe::{filter_by_uid, Aggregator};
use zbus::zvariant::{OwnedValue, Value};

const BUS_NAME: &str = "com.trafficview.Ebpf1";
const PATH: &str = "/com/trafficview/Ebpf1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Ebpf,
    AfPacket,
    Auto,
}

struct Helper {
    backend: &'static str,
    available: bool,
    reason: String,
    capabilities: Vec<String>,
    aggregator: Option<Arc<Mutex<Aggregator>>>,
}

#[zbus::interface(name = "com.trafficview.Ebpf1")]
impl Helper {
    /// 返回调用者 UID 可见的进程排行；每次调用刷新一次聚合。
    async fn get_processes(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<HashMap<String, OwnedValue>> {
        let Some(aggregator) = &self.aggregator else {
            return Ok(processes_reply(&[]));
        };
        let Some(caller) = caller_uid(connection, &header).await else {
            // 无法确认调用者身份时拒绝返回任何进程名。
            return Ok(processes_reply(&[]));
        };
        let samples = {
            let mut aggregator = aggregator
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            aggregator.refresh(Instant::now())
        };
        Ok(processes_reply(&filter_by_uid(samples, caller)))
    }

    async fn get_backend_status(&self) -> HashMap<String, OwnedValue> {
        let mut status = HashMap::new();
        status.insert("available".into(), owned(Value::from(self.available)));
        status.insert("backend".into(), owned(Value::from(self.backend)));
        status.insert("reason".into(), owned(Value::from(self.reason.clone())));
        status.insert(
            "capabilities".into(),
            owned(Value::from(self.capabilities.clone())),
        );
        status
    }
}

fn owned(value: Value<'static>) -> OwnedValue {
    OwnedValue::try_from(value).expect("helper value is owned")
}

fn processes_reply(samples: &[ProcessTrafficSample]) -> HashMap<String, OwnedValue> {
    let rows = samples
        .iter()
        .map(|sample| {
            let mut row: HashMap<String, Value<'static>> = HashMap::new();
            row.insert("pid".into(), Value::from(sample.pid));
            row.insert("name".into(), Value::from(sample.name.clone()));
            row.insert("rx_bps".into(), Value::from(sample.rx_bps));
            row.insert("tx_bps".into(), Value::from(sample.tx_bps));
            row.insert("rx_total".into(), Value::from(sample.rx_total));
            row.insert("tx_total".into(), Value::from(sample.tx_total));
            Value::from(row)
        })
        .collect::<Vec<Value<'static>>>();
    let mut reply = HashMap::new();
    reply.insert("processes".into(), owned(Value::from(rows)));
    reply
}

async fn caller_uid(
    connection: &zbus::Connection,
    header: &zbus::message::Header<'_>,
) -> Option<u32> {
    let sender = header.sender()?.to_string();
    let reply = connection
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetConnectionUnixUser",
            &(sender,),
        )
        .await
        .ok()?;
    reply.body().deserialize().ok()
}

fn open_af_packet() -> std::io::Result<libc::c_int> {
    let protocol = (libc::ETH_P_ALL as u16).to_be() as i32;
    let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_RAW, protocol) };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(fd)
    }
}

fn spawn_capture(fd: libc::c_int, aggregator: Arc<Mutex<Aggregator>>) {
    std::thread::spawn(move || {
        let mut buffer = vec![0u8; 65_536];
        loop {
            let read = unsafe { libc::recv(fd, buffer.as_mut_ptr().cast(), buffer.len(), 0) };
            if read > 0 {
                let now = Instant::now();
                let mut aggregator = aggregator
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                aggregator.ingest(&buffer[..read as usize], now);
            } else {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                tracing::warn!(%error, "AF_PACKET capture stopped");
                break;
            }
        }
    });
}

fn parse_args() -> anyhow::Result<Backend> {
    let mut args = std::env::args().skip(1);
    let mut backend = Backend::Auto;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--backend" => {
                let value = args.next().context("--backend requires a value")?;
                backend = match value.as_str() {
                    "ebpf" => Backend::Ebpf,
                    "af-packet" | "af_packet" => Backend::AfPacket,
                    "auto" => Backend::Auto,
                    other => anyhow::bail!("unknown backend '{other}'"),
                };
            }
            "-h" | "--help" => {
                println!(
                    "Usage: trafficviewd-ebpf [--backend ebpf|af-packet|auto]\n\
                     Serves com.trafficview.Ebpf1 on the system bus. AF_PACKET requires CAP_NET_RAW."
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument '{other}'"),
        }
    }
    Ok(backend)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let backend = parse_args()?;
    let (backend_name, available, reason, capabilities, aggregator) = match backend {
        Backend::Ebpf => (
            "ebpf",
            false,
            "eBPF backend is not built in this release; use --backend af-packet".to_string(),
            Vec::new(),
            None,
        ),
        Backend::AfPacket | Backend::Auto => match open_af_packet() {
            Ok(fd) => {
                let aggregator = Arc::new(Mutex::new(Aggregator::new(
                    PathBuf::from("/proc"),
                    Instant::now(),
                )));
                spawn_capture(fd, aggregator.clone());
                (
                    "af_packet",
                    true,
                    String::new(),
                    vec!["af_packet".to_string()],
                    Some(aggregator),
                )
            }
            Err(error) => (
                "af_packet",
                false,
                format!("CAP_NET_RAW is required for AF_PACKET: {error}"),
                vec!["af_packet".to_string()],
                None,
            ),
        },
    };

    let helper = Helper {
        backend: backend_name,
        available,
        reason,
        capabilities,
        aggregator,
    };
    let _connection = zbus::connection::Builder::system()
        .context("cannot connect to the system bus")?
        .name(BUS_NAME)?
        .serve_at(PATH, helper)?
        .build()
        .await?;
    tracing::info!(
        backend = backend_name,
        available,
        "trafficview process traffic helper is running"
    );
    std::future::pending::<()>().await;
    Ok(())
}
