//! D-Bus v1 冻结契约（M1）。
//!
//! BUS/PATH/IFACE 均为 `com.trafficview.Daemon1`（session bus）。

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};

use chrono::Utc;
use tokio::sync::RwLock;
use tracing::warn;
use trafficview_core::{
    config::{
        AlertRule, AlertsConfig, Config, ConfigStore, DisplayItem, HistoryConfig, NetworkSmoothing,
        OverlayConfig, PluginsConfig, PositionPreset, ProcessTrafficBackend, ProcessTrafficConfig,
        RateUnit, ThemeMode, SAMPLE_INTERVAL_MAX_MS, SAMPLE_INTERVAL_MIN_MS,
    },
    BatterySample, CpuSample, DiskSample, FilesystemSample, GpuSample, InterfaceInfo, MemSample,
    NetSample, ProcessTrafficSample, Snapshot, TempSample, TrafficSummary,
};
use zbus::{
    object_server::SignalContext,
    zvariant::{OwnedValue, Value},
};

use crate::service::SharedState;

pub const BUS_NAME: &str = "com.trafficview.Daemon1";
pub const PATH: &str = "/com/trafficview/Daemon1";
pub const INTERFACE: &str = "com.trafficview.Daemon1";

pub struct Daemon1 {
    state: Arc<RwLock<SharedState>>,
    store: ConfigStore,
}

impl Daemon1 {
    pub fn new(state: Arc<RwLock<SharedState>>, store: ConfigStore) -> Self {
        Self { state, store }
    }

    async fn persist(&self) {
        let config = self.state.read().await.config.clone();
        if let Err(error) = self.store.save(&config) {
            warn!(%error, "cannot persist configuration");
        }
    }
}

#[zbus::interface(name = "com.trafficview.Daemon1")]
impl Daemon1 {
    #[zbus(property)]
    async fn sample_interval(&self) -> u32 {
        self.state.read().await.config.sample_interval_ms
    }

    #[zbus(property(emits_changed_signal = "false"))]
    async fn interfaces(&self) -> Vec<String> {
        self.state.read().await.interfaces.clone()
    }

    #[zbus(property)]
    async fn active_interface(&self) -> String {
        self.state.read().await.config.active_interface.clone()
    }

    /// M3 新增只读属性：`light` 或 `dark`，由 portal 与用户配置共同决定。
    #[zbus(property)]
    async fn color_scheme(&self) -> String {
        crate::theme::scheme_name(self.state.read().await.color_scheme).to_owned()
    }

    /// M4 新增只读属性：暂停状态。暂停时 `GetSnapshot` 会报错，
    /// 前端据此区分“暂停”和“daemon 已退出”，不能把暂停当作离线。
    #[zbus(property(emits_changed_signal = "false"))]
    async fn paused(&self) -> bool {
        self.state.read().await.paused
    }

    #[zbus(property(emits_changed_signal = "false"))]
    async fn overlay_visible(&self) -> bool {
        self.state.read().await.config.overlay.visible
    }

    #[zbus(property(emits_changed_signal = "false"))]
    async fn overlay_show_in_taskbar(&self) -> bool {
        self.state.read().await.config.overlay.show_in_taskbar
    }

    /// M4 新增只读属性：各采集器的可用性；值含 `available` 与 `reason`。
    #[zbus(property(emits_changed_signal = "false"))]
    async fn collector_status(&self) -> HashMap<String, OwnedValue> {
        self.state
            .read()
            .await
            .collector_status
            .iter()
            .map(|(name, status)| {
                let mut entry: HashMap<String, Value<'static>> = HashMap::new();
                entry.insert("available".into(), Value::from(status.available));
                entry.insert("reason".into(), Value::from(status.reason.clone()));
                (name.clone(), owned(Value::from(entry)))
            })
            .collect()
    }

    /// M4 新增：完整配置快照；键与 `UpdateConfig` 对称。
    async fn get_config(&self) -> HashMap<String, OwnedValue> {
        config_to_dbus(&self.state.read().await.config)
    }

    /// M4 新增：按顶层键部分更新。先在副本上合并并校验，保存成功后才提交；
    /// 任一字段非法时整体不生效。设置界面不得直接写配置文件。
    async fn update_config(
        &self,
        #[zbus(signal_context)] context: SignalContext<'_>,
        updates: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        let mut state = self.state.write().await;
        let previous_backend = state.config.process_traffic.backend;
        let previous_interval_ms = state.config.sample_interval_ms;
        let mut config = state.config.clone();
        apply_config_update(&mut config, &updates).map_err(zbus::fdo::Error::InvalidArgs)?;
        config.validate().map_err(zbus::fdo::Error::InvalidArgs)?;
        if !config.active_interface.is_empty()
            && !state
                .interfaces
                .iter()
                .any(|iface| iface == &config.active_interface)
        {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "configured interface {} does not exist",
                config.active_interface
            )));
        }
        // 持锁写盘：两个并发 UpdateConfig 不会产生部分 TOML 或交错状态。
        if let Err(error) = self.store.save(&config) {
            return Err(zbus::fdo::Error::Failed(error.to_string()));
        }
        let rules = config.alerts.rules.clone();
        let plugins_enabled = config.plugins.enabled;
        let process_backend = config.process_traffic.backend;
        let process_interval =
            Duration::from_millis(u64::from(config.sample_interval_ms.max(1000)));
        let process_changed = process_backend != previous_backend
            || config.sample_interval_ms != previous_interval_ms;
        state.config = config;
        state.color_scheme = crate::theme::decide(state.config.theme, state.portal_scheme);
        if let Ok(mut engine) = state.alerts.lock() {
            engine.reload(&rules);
        }
        let supervisor = state.plugin_supervisor.clone();
        let process_traffic = state.process_traffic.clone();
        drop(state);
        if let Some(supervisor) = supervisor {
            supervisor.reload(plugins_enabled);
        }
        if let Some(monitor) = process_traffic {
            if process_changed {
                monitor.restart(process_backend, process_interval);
            }
        }
        let _ = Daemon1::config_changed(&context).await;
        Ok(())
    }

    async fn get_snapshot(&self) -> zbus::fdo::Result<HashMap<String, OwnedValue>> {
        let state = self.state.read().await;
        if state.paused {
            return Err(zbus::fdo::Error::Failed("daemon is paused".into()));
        }
        let snapshot = state
            .snapshot
            .as_ref()
            .ok_or_else(|| zbus::fdo::Error::Failed("no sample available yet".into()))?;
        Ok(snapshot_to_dbus(snapshot))
    }

    /// M3 新增：按日/月查询历史。`iface_stable` 为空表示所有网卡求和。
    async fn get_traffic_summary(
        &self,
        scope: &str,
        iface_stable: &str,
    ) -> zbus::fdo::Result<HashMap<String, OwnedValue>> {
        if !matches!(scope, "day" | "month") {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "unsupported scope '{scope}'; use day or month"
            )));
        }
        let history = self.state.read().await.history.clone();
        let Some(history) = history else {
            return Err(zbus::fdo::Error::Failed(
                "traffic history is unavailable".into(),
            ));
        };
        let stable = (!iface_stable.is_empty()).then_some(iface_stable);
        let summary = {
            let store = history
                .lock()
                .map_err(|_| zbus::fdo::Error::Failed("history mutex is poisoned".into()))?;
            store
                .summary(scope, stable, Utc::now())
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
        };
        Ok(traffic_summary_to_dbus(&summary))
    }

    /// M3 新增：网卡详情；键为接口名，值为 `a{sv}`。原 `Interfaces` 属性保持兼容。
    async fn get_interface_details(&self) -> HashMap<String, OwnedValue> {
        let state = self.state.read().await;
        interface_details_to_dbus(&state.interface_details, state.active_stable.as_deref())
    }

    async fn set_active_interface(
        &self,
        #[zbus(signal_context)] context: SignalContext<'_>,
        name: String,
    ) -> zbus::fdo::Result<()> {
        {
            let state = self.state.read().await;
            if !name.is_empty() && !state.interfaces.iter().any(|iface| iface == &name) {
                return Err(zbus::fdo::Error::Failed(format!(
                    "interface {name} does not exist"
                )));
            }
        }
        self.state.write().await.config.active_interface = name;
        self.persist().await;
        let _ = Daemon1::config_changed(&context).await;
        Ok(())
    }

    async fn set_interval(
        &self,
        #[zbus(signal_context)] context: SignalContext<'_>,
        ms: u32,
    ) -> zbus::fdo::Result<()> {
        if !(SAMPLE_INTERVAL_MIN_MS..=SAMPLE_INTERVAL_MAX_MS).contains(&ms) {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "interval must be within {SAMPLE_INTERVAL_MIN_MS}..={SAMPLE_INTERVAL_MAX_MS} ms"
            )));
        }
        self.state.write().await.config.sample_interval_ms = ms;
        self.persist().await;
        let _ = Daemon1::config_changed(&context).await;
        Ok(())
    }

    async fn set_overlay_visible(
        &self,
        #[zbus(signal_context)] context: SignalContext<'_>,
        visible: bool,
    ) -> zbus::fdo::Result<()> {
        self.state.write().await.config.overlay.visible = visible;
        self.persist().await;
        let _ = Daemon1::config_changed(&context).await;
        Ok(())
    }

    async fn set_overlay_show_in_taskbar(
        &self,
        #[zbus(signal_context)] context: SignalContext<'_>,
        show: bool,
    ) -> zbus::fdo::Result<()> {
        self.state.write().await.config.overlay.show_in_taskbar = show;
        self.persist().await;
        let _ = Daemon1::config_changed(&context).await;
        Ok(())
    }

    async fn reload_config(
        &self,
        #[zbus(signal_context)] context: SignalContext<'_>,
    ) -> zbus::fdo::Result<()> {
        let config = self
            .store
            .load()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        let interfaces = self.state.read().await.interfaces.clone();
        if !config.active_interface.is_empty()
            && !interfaces
                .iter()
                .any(|iface| iface == &config.active_interface)
        {
            return Err(zbus::fdo::Error::Failed(format!(
                "configured interface {} does not exist",
                config.active_interface
            )));
        }
        let rules = config.alerts.rules.clone();
        let plugins_enabled = config.plugins.enabled;
        let supervisor = {
            let mut state = self.state.write().await;
            state.config = config;
            state.color_scheme = crate::theme::decide(state.config.theme, state.portal_scheme);
            if let Ok(mut engine) = state.alerts.lock() {
                engine.reload(&rules);
            };
            state.plugin_supervisor.clone()
        };
        // ReloadConfig 是唯一允许被禁用插件重试的入口。
        if let Some(supervisor) = supervisor {
            supervisor.reload(plugins_enabled);
        }
        let _ = Daemon1::config_changed(&context).await;
        Ok(())
    }

    async fn pause(
        &self,
        #[zbus(signal_context)] context: SignalContext<'_>,
        paused: bool,
    ) -> zbus::fdo::Result<()> {
        self.state.write().await.paused = paused;
        let _ = Daemon1::config_changed(&context).await;
        Ok(())
    }

    async fn quit(&self) -> zbus::fdo::Result<()> {
        let _ = self.state.read().await.shutdown.send(true);
        Ok(())
    }

    #[zbus(signal)]
    pub async fn sample(
        context: &SignalContext<'_>,
        snapshot: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn alert(
        context: &SignalContext<'_>,
        level: String,
        code: String,
        message: String,
    ) -> zbus::Result<()>;

    #[zbus(signal, name = "InterfacesChanged")]
    pub async fn notify_interfaces(
        context: &SignalContext<'_>,
        interfaces: Vec<String>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn config_changed(context: &SignalContext<'_>) -> zbus::Result<()>;
}

fn owned(value: Value<'static>) -> OwnedValue {
    OwnedValue::try_from(value).expect("constructed D-Bus values are owned")
}

fn net_value(sample: &NetSample) -> Value<'static> {
    let mut map: HashMap<String, Value<'static>> = HashMap::new();
    map.insert("iface".into(), Value::from(sample.iface.clone()));
    map.insert("rx_bps".into(), Value::from(sample.rx_bps));
    map.insert("tx_bps".into(), Value::from(sample.tx_bps));
    map.insert("rx_total".into(), Value::from(sample.rx_total));
    map.insert("tx_total".into(), Value::from(sample.tx_total));
    Value::from(map)
}

fn cpu_value(sample: &CpuSample) -> Value<'static> {
    let mut map: HashMap<String, Value<'static>> = HashMap::new();
    map.insert("total_pct".into(), Value::from(sample.total_pct));
    map.insert("per_core".into(), Value::from(sample.per_core.clone()));
    if let Some(freq) = sample.freq_mhz {
        map.insert("freq_mhz".into(), Value::from(freq));
    }
    if let Some(load) = sample.load {
        map.insert("load".into(), Value::from(load.to_vec()));
    }
    Value::from(map)
}

fn mem_value(sample: &MemSample) -> Value<'static> {
    let mut map: HashMap<String, Value<'static>> = HashMap::new();
    map.insert("used_pct".into(), Value::from(sample.used_pct));
    map.insert("total".into(), Value::from(sample.total));
    map.insert("available".into(), Value::from(sample.available));
    map.insert("swap_pct".into(), Value::from(sample.swap_pct));
    Value::from(map)
}

/// 顶层必需键 `ts_ms/net/cpu/mem`；disk/gpu 未采集时省略；temps/plugin 为空集合。
pub fn snapshot_to_dbus(snapshot: &Snapshot) -> HashMap<String, OwnedValue> {
    let mut top: HashMap<String, OwnedValue> = HashMap::new();
    top.insert("ts_ms".into(), owned(Value::from(snapshot.ts_ms)));
    top.insert("net".into(), owned(net_value(&snapshot.net)));
    top.insert("cpu".into(), owned(cpu_value(&snapshot.cpu)));
    top.insert("mem".into(), owned(mem_value(&snapshot.mem)));
    if let Some(disk) = snapshot.disk.as_ref() {
        top.insert("disk".into(), owned(disk_value(disk)));
    }
    if let Some(gpu) = snapshot.gpu.as_ref() {
        top.insert("gpu".into(), owned(gpu_value(gpu)));
    }
    if !snapshot.gpus.is_empty() {
        top.insert(
            "gpus".into(),
            owned(Value::from(
                snapshot
                    .gpus
                    .iter()
                    .map(gpu_value)
                    .collect::<Vec<Value<'static>>>(),
            )),
        );
    }
    top.insert(
        "temps".into(),
        owned(Value::from(
            snapshot
                .temps
                .iter()
                .map(temp_value)
                .collect::<Vec<Value<'static>>>(),
        )),
    );
    if let Some(cpu_temp) = snapshot.cpu_temp {
        top.insert("cpu_temp".into(), owned(Value::from(f64::from(cpu_temp))));
    }
    if let Some(filesystems) = &snapshot.filesystems {
        top.insert(
            "filesystems".into(),
            owned(Value::from(
                filesystems
                    .iter()
                    .map(filesystem_value)
                    .collect::<Vec<Value<'static>>>(),
            )),
        );
    }
    if let Some(battery) = &snapshot.battery {
        top.insert("battery".into(), owned(battery_value(battery)));
    }
    if let Some(uptime_s) = snapshot.uptime_s {
        top.insert("uptime_s".into(), owned(Value::from(uptime_s)));
    }
    if let Some(processes) = &snapshot.processes {
        top.insert(
            "processes".into(),
            owned(Value::from(
                processes
                    .iter()
                    .map(process_value)
                    .collect::<Vec<Value<'static>>>(),
            )),
        );
    }
    top.insert("plugin".into(), owned(plugin_value(&snapshot.plugin)));
    top
}

fn process_value(sample: &ProcessTrafficSample) -> Value<'static> {
    let mut map: HashMap<String, Value<'static>> = HashMap::new();
    map.insert("pid".into(), Value::from(sample.pid));
    map.insert("name".into(), Value::from(sample.name.clone()));
    map.insert("rx_bps".into(), Value::from(sample.rx_bps));
    map.insert("tx_bps".into(), Value::from(sample.tx_bps));
    map.insert("rx_total".into(), Value::from(sample.rx_total));
    map.insert("tx_total".into(), Value::from(sample.tx_total));
    Value::from(map)
}

fn filesystem_value(sample: &FilesystemSample) -> Value<'static> {
    let mut map: HashMap<String, Value<'static>> = HashMap::new();
    map.insert("mount".into(), Value::from(sample.mount.clone()));
    map.insert("total".into(), Value::from(sample.total));
    map.insert("available".into(), Value::from(sample.available));
    map.insert("used_pct".into(), Value::from(sample.used_pct));
    Value::from(map)
}

fn battery_value(sample: &BatterySample) -> Value<'static> {
    let mut map: HashMap<String, Value<'static>> = HashMap::new();
    map.insert("percent".into(), Value::from(sample.percent));
    map.insert("status".into(), Value::from(sample.status.clone()));
    Value::from(map)
}

fn disk_value(sample: &DiskSample) -> Value<'static> {
    let mut map: HashMap<String, Value<'static>> = HashMap::new();
    map.insert("read_bps".into(), Value::from(sample.read_bps));
    map.insert("write_bps".into(), Value::from(sample.write_bps));
    Value::from(map)
}

fn gpu_value(sample: &GpuSample) -> Value<'static> {
    let mut map: HashMap<String, Value<'static>> = HashMap::new();
    map.insert("name".into(), Value::from(sample.name.clone()));
    map.insert("util_pct".into(), Value::from(sample.util_pct));
    map.insert("mem_used".into(), Value::from(sample.mem_used));
    map.insert("mem_total".into(), Value::from(sample.mem_total));
    if let Some(freq_mhz) = sample.freq_mhz {
        map.insert("freq_mhz".into(), Value::from(freq_mhz));
    }
    map.insert("stale".into(), Value::from(sample.stale));
    if let Some(observed_at_ms) = sample.observed_at_ms {
        map.insert("observed_at_ms".into(), Value::from(observed_at_ms));
    }
    Value::from(map)
}

fn temp_value(sample: &TempSample) -> Value<'static> {
    let mut map: HashMap<String, Value<'static>> = HashMap::new();
    map.insert("label".into(), Value::from(sample.label.clone()));
    map.insert("celsius".into(), Value::from(sample.celsius));
    Value::from(map)
}

fn plugin_value(plugin: &BTreeMap<String, String>) -> Value<'static> {
    let map: HashMap<String, String> = plugin
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    Value::from(map)
}

/// `GetTrafficSummary` 的固定返回键：scope/start/end/rx/tx/interfaces。
pub fn traffic_summary_to_dbus(summary: &TrafficSummary) -> HashMap<String, OwnedValue> {
    let interfaces = summary
        .interfaces
        .iter()
        .map(|iface| {
            let mut map: HashMap<String, Value<'static>> = HashMap::new();
            map.insert("stable".into(), Value::from(iface.stable.clone()));
            map.insert("name".into(), Value::from(iface.name.clone()));
            map.insert("rx".into(), Value::from(iface.rx));
            map.insert("tx".into(), Value::from(iface.tx));
            Value::from(map)
        })
        .collect::<Vec<Value<'static>>>();
    let mut top: HashMap<String, OwnedValue> = HashMap::new();
    top.insert("scope".into(), owned(Value::from(summary.scope.clone())));
    top.insert("start".into(), owned(Value::from(summary.start.clone())));
    top.insert("end".into(), owned(Value::from(summary.end.clone())));
    top.insert("rx".into(), owned(Value::from(summary.rx)));
    top.insert("tx".into(), owned(Value::from(summary.tx)));
    top.insert("interfaces".into(), owned(Value::from(interfaces)));
    top
}

fn interface_details_to_dbus(
    details: &[InterfaceInfo],
    active_stable: Option<&str>,
) -> HashMap<String, OwnedValue> {
    let mut map: HashMap<String, OwnedValue> = HashMap::new();
    for iface in details {
        let mut entry: HashMap<String, Value<'static>> = HashMap::new();
        entry.insert("stable".into(), Value::from(iface.stable.clone()));
        entry.insert(
            "mac".into(),
            Value::from(iface.mac.clone().unwrap_or_default()),
        );
        entry.insert("ifindex".into(), Value::from(iface.ifindex));
        entry.insert("operstate".into(), Value::from(iface.operstate.clone()));
        entry.insert("carrier".into(), Value::from(iface.carrier));
        entry.insert("loopback".into(), Value::from(iface.is_loopback));
        entry.insert(
            "active".into(),
            Value::from(active_stable == Some(iface.stable.as_str())),
        );
        map.insert(iface.name.clone(), owned(Value::from(entry)));
    }
    map
}

/// 从 D-Bus 字典还原 `Snapshot`；省略的 disk/gpu 保持 `None`。
pub fn snapshot_from_dbus(dict: &HashMap<String, OwnedValue>) -> anyhow::Result<Snapshot> {
    let ts_ms = u64::try_from(required(dict, "ts_ms")?)?;
    let net = nested(dict, "net")?;
    let cpu = nested(dict, "cpu")?;
    let mem = nested(dict, "mem")?;

    let net = NetSample {
        iface: string(&net, "iface")?,
        rx_bps: u64::try_from(required(&net, "rx_bps")?)?,
        tx_bps: u64::try_from(required(&net, "tx_bps")?)?,
        rx_total: u64::try_from(required(&net, "rx_total")?)?,
        tx_total: u64::try_from(required(&net, "tx_total")?)?,
    };
    let per_core = Vec::<f64>::try_from(required(&cpu, "per_core")?.try_clone()?)?
        .into_iter()
        .map(|value| value as f32)
        .collect();
    let freq_mhz = cpu
        .get("freq_mhz")
        .map(|value| Ok::<u32, anyhow::Error>(u32::try_from(value.try_clone()?)?))
        .transpose()?;
    let load = cpu
        .get("load")
        .map(|value| -> anyhow::Result<[f32; 3]> {
            let values = Vec::<f64>::try_from(value.try_clone()?)?;
            let mut load = [0.0; 3];
            for (slot, value) in load.iter_mut().zip(values) {
                *slot = value as f32;
            }
            Ok(load)
        })
        .transpose()?;
    let cpu = CpuSample {
        total_pct: f64::try_from(required(&cpu, "total_pct")?.try_clone()?)? as f32,
        per_core,
        freq_mhz,
        load,
    };
    let mem = MemSample {
        used_pct: f64::try_from(required(&mem, "used_pct")?.try_clone()?)? as f32,
        total: u64::try_from(required(&mem, "total")?)?,
        available: u64::try_from(required(&mem, "available")?)?,
        swap_pct: f64::try_from(required(&mem, "swap_pct")?.try_clone()?)? as f32,
    };

    let disk = dict
        .get("disk")
        .map(|value| -> anyhow::Result<DiskSample> {
            let disk: HashMap<String, OwnedValue> = value.try_clone()?.try_into()?;
            Ok(DiskSample {
                read_bps: u64::try_from(required(&disk, "read_bps")?)?,
                write_bps: u64::try_from(required(&disk, "write_bps")?)?,
            })
        })
        .transpose()?;
    let gpu = dict
        .get("gpu")
        .map(|value| -> anyhow::Result<GpuSample> {
            let gpu: HashMap<String, OwnedValue> = value.try_clone()?.try_into()?;
            gpu_from_dbus(&gpu)
        })
        .transpose()?;
    let mut gpus = Vec::new();
    if let Some(value) = dict.get("gpus") {
        for entry in Vec::<OwnedValue>::try_from(value.try_clone()?)? {
            let gpu: HashMap<String, OwnedValue> = entry.try_into()?;
            gpus.push(gpu_from_dbus(&gpu)?);
        }
    }
    // `gpu` 是主 GPU 兼容别名；只发送 `gpus` 的对端也能正确还原。
    let gpu = gpu.or_else(|| gpus.first().cloned());

    let mut temps = Vec::new();
    if let Some(value) = dict.get("temps") {
        for entry in Vec::<OwnedValue>::try_from(value.try_clone()?)? {
            let temp: HashMap<String, OwnedValue> = entry.try_into()?;
            temps.push(TempSample {
                label: string(&temp, "label")?,
                celsius: f64::try_from(required(&temp, "celsius")?.try_clone()?)? as f32,
            });
        }
    }
    let cpu_temp = dict
        .get("cpu_temp")
        .map(|value| -> anyhow::Result<f32> { Ok(f64::try_from(value.try_clone()?)? as f32) })
        .transpose()?;
    let filesystems = dict
        .get("filesystems")
        .map(|value| -> anyhow::Result<Vec<FilesystemSample>> {
            let entries: Vec<HashMap<String, OwnedValue>> = value.try_clone()?.try_into()?;
            entries
                .into_iter()
                .map(|entry| {
                    Ok(FilesystemSample {
                        mount: string(&entry, "mount")?,
                        total: u64::try_from(required(&entry, "total")?.try_clone()?)?,
                        available: u64::try_from(required(&entry, "available")?.try_clone()?)?,
                        used_pct: f64::try_from(required(&entry, "used_pct")?.try_clone()?)? as f32,
                    })
                })
                .collect()
        })
        .transpose()?;
    let battery = dict
        .get("battery")
        .map(|value| -> anyhow::Result<BatterySample> {
            let entry: HashMap<String, OwnedValue> = value.try_clone()?.try_into()?;
            Ok(BatterySample {
                percent: f64::try_from(required(&entry, "percent")?.try_clone()?)? as f32,
                status: string(&entry, "status")?,
            })
        })
        .transpose()?;
    let uptime_s = dict
        .get("uptime_s")
        .map(|value| -> anyhow::Result<u64> { Ok(u64::try_from(value.try_clone()?)?) })
        .transpose()?;
    let plugin = dict
        .get("plugin")
        .map(|value| -> anyhow::Result<BTreeMap<String, String>> {
            let map: HashMap<String, String> = value.try_clone()?.try_into()?;
            Ok(map.into_iter().collect())
        })
        .transpose()?
        .unwrap_or_default();
    let processes = dict
        .get("processes")
        .map(|value| -> anyhow::Result<Vec<ProcessTrafficSample>> {
            let entries: Vec<HashMap<String, OwnedValue>> = value.try_clone()?.try_into()?;
            entries
                .into_iter()
                .map(|entry| {
                    Ok(ProcessTrafficSample {
                        pid: u32::try_from(required(&entry, "pid")?.try_clone()?)?,
                        name: string(&entry, "name")?,
                        rx_bps: u64::try_from(required(&entry, "rx_bps")?.try_clone()?)?,
                        tx_bps: u64::try_from(required(&entry, "tx_bps")?.try_clone()?)?,
                        rx_total: u64::try_from(required(&entry, "rx_total")?.try_clone()?)?,
                        tx_total: u64::try_from(required(&entry, "tx_total")?.try_clone()?)?,
                    })
                })
                .collect()
        })
        .transpose()?;

    Ok(Snapshot {
        ts_ms,
        net,
        cpu,
        mem,
        disk,
        gpu,
        gpus,
        cpu_temp,
        temps,
        filesystems,
        battery,
        uptime_s,
        processes,
        plugin,
    })
}

fn required<'a>(
    dict: &'a HashMap<String, OwnedValue>,
    key: &str,
) -> anyhow::Result<&'a OwnedValue> {
    dict.get(key)
        .ok_or_else(|| anyhow::anyhow!("missing snapshot key: {key}"))
}

fn nested(
    dict: &HashMap<String, OwnedValue>,
    key: &str,
) -> anyhow::Result<HashMap<String, OwnedValue>> {
    Ok(required(dict, key)?.try_clone()?.try_into()?)
}

fn string(dict: &HashMap<String, OwnedValue>, key: &str) -> anyhow::Result<String> {
    Ok(String::try_from(required(dict, key)?.try_clone()?)?)
}

fn gpu_from_dbus(dict: &HashMap<String, OwnedValue>) -> anyhow::Result<GpuSample> {
    let freq_mhz = dict
        .get("freq_mhz")
        .map(|value| u32::try_from(value.try_clone()?))
        .transpose()?;
    let stale = dict
        .get("stale")
        .map(|value| bool::try_from(value.try_clone()?))
        .transpose()?
        .unwrap_or(false);
    let observed_at_ms = dict
        .get("observed_at_ms")
        .map(|value| u64::try_from(value.try_clone()?))
        .transpose()?;
    Ok(GpuSample {
        name: string(dict, "name")?,
        util_pct: f64::try_from(required(dict, "util_pct")?.try_clone()?)? as f32,
        mem_used: u64::try_from(required(dict, "mem_used")?.try_clone()?)?,
        mem_total: u64::try_from(required(dict, "mem_total")?.try_clone()?)?,
        freq_mhz,
        temp_celsius: None,
        stale,
        observed_at_ms,
    })
}

// ---------------------------------------------------------------------------
// M4 配置 API：`GetConfig`/`UpdateConfig` 的 a{sv} 映射。
// 顶层键部分更新；嵌套小节按键合并；数组（rules/items）整体替换。
// ---------------------------------------------------------------------------

fn smoothing_name(value: NetworkSmoothing) -> &'static str {
    match value {
        NetworkSmoothing::None => "none",
        NetworkSmoothing::MovingAverage3 => "moving_average_3",
        NetworkSmoothing::Ema => "ema",
    }
}

fn theme_name(value: ThemeMode) -> &'static str {
    match value {
        ThemeMode::Auto => "auto",
        ThemeMode::Light => "light",
        ThemeMode::Dark => "dark",
    }
}

fn unit_name(value: RateUnit) -> &'static str {
    match value {
        RateUnit::Bytes => "bytes",
        RateUnit::Bits => "bits",
    }
}

/// 完整配置导出；与 `apply_config_update` 的键一一对应。
pub fn config_to_dbus(config: &Config) -> HashMap<String, OwnedValue> {
    let mut top: HashMap<String, OwnedValue> = HashMap::new();
    top.insert(
        "sample_interval_ms".into(),
        owned(Value::from(config.sample_interval_ms)),
    );
    top.insert(
        "active_interface".into(),
        owned(Value::from(config.active_interface.clone())),
    );
    top.insert(
        "memory_include_cache".into(),
        owned(Value::from(config.memory_include_cache)),
    );
    top.insert(
        "interface_blacklist".into(),
        owned(Value::from(config.interface_blacklist.clone())),
    );
    top.insert(
        "interface_whitelist".into(),
        owned(Value::from(config.interface_whitelist.clone())),
    );
    top.insert(
        "network_smoothing".into(),
        owned(Value::from(smoothing_name(config.network_smoothing))),
    );
    top.insert("theme".into(), owned(Value::from(theme_name(config.theme))));
    top.insert("history".into(), owned(history_value(&config.history)));
    top.insert("alerts".into(), owned(alerts_value(&config.alerts)));
    top.insert("overlay".into(), owned(overlay_value(&config.overlay)));
    top.insert("plugins".into(), owned(plugins_value(&config.plugins)));
    top.insert(
        "process_traffic".into(),
        owned(process_traffic_value(&config.process_traffic)),
    );
    top
}

fn history_value(config: &HistoryConfig) -> Value<'static> {
    let mut map: HashMap<String, Value<'static>> = HashMap::new();
    map.insert("retention_days".into(), Value::from(config.retention_days));
    map.insert("timezone".into(), Value::from(config.timezone.clone()));
    Value::from(map)
}

fn alerts_value(config: &AlertsConfig) -> Value<'static> {
    let rules = config
        .rules
        .iter()
        .map(|rule| {
            let mut map: HashMap<String, Value<'static>> = HashMap::new();
            map.insert("enabled".into(), Value::from(rule.enabled));
            map.insert("metric".into(), Value::from(rule.metric.clone()));
            map.insert("op".into(), Value::from(rule.op.clone()));
            map.insert("threshold".into(), Value::from(rule.threshold));
            map.insert("duration_s".into(), Value::from(rule.duration_s));
            map.insert("cooldown_s".into(), Value::from(rule.cooldown_s));
            map.insert("quiet_hours".into(), Value::from(rule.quiet_hours.clone()));
            map.insert("level".into(), Value::from(rule.level.clone()));
            Value::from(map)
        })
        .collect::<Vec<Value<'static>>>();
    let mut map: HashMap<String, Value<'static>> = HashMap::new();
    map.insert("rules".into(), Value::from(rules));
    Value::from(map)
}

fn overlay_value(config: &OverlayConfig) -> Value<'static> {
    let items = config
        .items
        .iter()
        .map(|item| {
            let mut map: HashMap<String, Value<'static>> = HashMap::new();
            map.insert("id".into(), Value::from(item.id.clone()));
            map.insert("label".into(), Value::from(item.label.clone()));
            map.insert("unit".into(), Value::from(item.unit.clone()));
            map.insert("format".into(), Value::from(item.format.clone()));
            map.insert("color".into(), Value::from(item.color.clone()));
            map.insert("visible".into(), Value::from(item.visible));
            Value::from(map)
        })
        .collect::<Vec<Value<'static>>>();
    let mut map: HashMap<String, Value<'static>> = HashMap::new();
    map.insert("visible".into(), Value::from(config.visible));
    map.insert("position".into(), Value::from(config.position.to_vec()));
    map.insert(
        "position_preset".into(),
        Value::from(preset_name(config.position_preset)),
    );
    map.insert("opacity".into(), Value::from(f64::from(config.opacity)));
    map.insert("click_through".into(), Value::from(config.click_through));
    map.insert("always_on_top".into(), Value::from(config.always_on_top));
    map.insert("skin".into(), Value::from(config.skin.clone()));
    map.insert("units".into(), Value::from(unit_name(config.units)));
    map.insert("items".into(), Value::from(items));
    Value::from(map)
}

fn plugins_value(config: &PluginsConfig) -> Value<'static> {
    let mut map: HashMap<String, Value<'static>> = HashMap::new();
    map.insert("enabled".into(), Value::from(config.enabled));
    Value::from(map)
}

fn backend_name(value: ProcessTrafficBackend) -> &'static str {
    match value {
        ProcessTrafficBackend::Off => "off",
        ProcessTrafficBackend::Ebpf => "ebpf",
        ProcessTrafficBackend::AfPacket => "af_packet",
        ProcessTrafficBackend::Auto => "auto",
    }
}

fn process_traffic_value(config: &ProcessTrafficConfig) -> Value<'static> {
    let mut map: HashMap<String, Value<'static>> = HashMap::new();
    map.insert("backend".into(), Value::from(backend_name(config.backend)));
    Value::from(map)
}

fn cloned(value: &OwnedValue, path: &str) -> Result<OwnedValue, String> {
    value
        .try_clone()
        .map_err(|error| format!("{path}: cannot clone value: {error}"))
}

fn as_u32(value: OwnedValue, path: &str) -> Result<u32, String> {
    u32::try_from(value).map_err(|error| format!("{path}: expected u32: {error}"))
}

fn as_f64(value: OwnedValue, path: &str) -> Result<f64, String> {
    f64::try_from(value).map_err(|error| format!("{path}: expected f64: {error}"))
}

fn as_bool(value: OwnedValue, path: &str) -> Result<bool, String> {
    bool::try_from(value).map_err(|error| format!("{path}: expected bool: {error}"))
}

fn as_string(value: OwnedValue, path: &str) -> Result<String, String> {
    String::try_from(value).map_err(|error| format!("{path}: expected string: {error}"))
}

fn as_strings(value: OwnedValue, path: &str) -> Result<Vec<String>, String> {
    Vec::<String>::try_from(value).map_err(|error| format!("{path}: expected as: {error}"))
}

fn as_map(value: OwnedValue, path: &str) -> Result<HashMap<String, OwnedValue>, String> {
    value
        .try_into()
        .map_err(|error| format!("{path}: expected a{{sv}}: {error}"))
}

fn as_variants(value: OwnedValue, path: &str) -> Result<Vec<HashMap<String, OwnedValue>>, String> {
    let entries = Vec::<OwnedValue>::try_from(value)
        .map_err(|error| format!("{path}: expected av: {error}"))?;
    entries
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            HashMap::<String, OwnedValue>::try_from(entry)
                .map_err(|error| format!("{path}[{index}]: expected a{{sv}}: {error}"))
        })
        .collect()
}

fn parse_smoothing(value: &str) -> Result<NetworkSmoothing, String> {
    match value {
        "none" => Ok(NetworkSmoothing::None),
        "moving_average_3" | "moving_average3" => Ok(NetworkSmoothing::MovingAverage3),
        "ema" => Ok(NetworkSmoothing::Ema),
        other => Err(format!(
            "network_smoothing '{other}' must be none, moving_average_3 or ema"
        )),
    }
}

fn parse_theme(value: &str) -> Result<ThemeMode, String> {
    match value {
        "auto" => Ok(ThemeMode::Auto),
        "light" => Ok(ThemeMode::Light),
        "dark" => Ok(ThemeMode::Dark),
        other => Err(format!("theme '{other}' must be auto, light or dark")),
    }
}

fn parse_units(value: &str) -> Result<RateUnit, String> {
    match value {
        "bytes" => Ok(RateUnit::Bytes),
        "bits" => Ok(RateUnit::Bits),
        other => Err(format!("overlay.units '{other}' must be bytes or bits")),
    }
}

fn preset_name(value: PositionPreset) -> &'static str {
    match value {
        PositionPreset::BottomRight => "bottom_right",
        PositionPreset::BottomLeft => "bottom_left",
        PositionPreset::TopRight => "top_right",
        PositionPreset::TopLeft => "top_left",
        PositionPreset::Manual => "manual",
    }
}

fn parse_preset(value: &str) -> Result<PositionPreset, String> {
    match value {
        "bottom_right" => Ok(PositionPreset::BottomRight),
        "bottom_left" => Ok(PositionPreset::BottomLeft),
        "top_right" => Ok(PositionPreset::TopRight),
        "top_left" => Ok(PositionPreset::TopLeft),
        "manual" => Ok(PositionPreset::Manual),
        other => Err(format!(
            "overlay.position_preset '{other}' must be bottom_right, bottom_left, top_right, top_left or manual"
        )),
    }
}

fn parse_backend(value: &str) -> Result<ProcessTrafficBackend, String> {
    match value {
        "off" => Ok(ProcessTrafficBackend::Off),
        "ebpf" => Ok(ProcessTrafficBackend::Ebpf),
        "af_packet" => Ok(ProcessTrafficBackend::AfPacket),
        "auto" => Ok(ProcessTrafficBackend::Auto),
        other => Err(format!(
            "process_traffic.backend '{other}' must be off, ebpf, af_packet or auto"
        )),
    }
}

fn as_position(value: OwnedValue, path: &str) -> Result<[i32; 2], String> {
    let values =
        Vec::<i32>::try_from(value).map_err(|error| format!("{path}: expected ai: {error}"))?;
    let [x, y] = values.as_slice() else {
        return Err(format!("{path}: expected exactly 2 integers"));
    };
    Ok([*x, *y])
}

/// 在 `config` 副本上合并顶层部分更新；未知键或类型错误返回可展示原因。
pub fn apply_config_update(
    config: &mut Config,
    updates: &HashMap<String, OwnedValue>,
) -> Result<(), String> {
    for (key, value) in updates {
        match key.as_str() {
            "sample_interval_ms" => {
                config.sample_interval_ms = as_u32(cloned(value, key)?, key)?;
            }
            "active_interface" => {
                config.active_interface = as_string(cloned(value, key)?, key)?;
            }
            "memory_include_cache" => {
                config.memory_include_cache = as_bool(cloned(value, key)?, key)?;
            }
            "interface_blacklist" => {
                config.interface_blacklist = as_strings(cloned(value, key)?, key)?;
            }
            "interface_whitelist" => {
                config.interface_whitelist = as_strings(cloned(value, key)?, key)?;
            }
            "network_smoothing" => {
                let text = as_string(cloned(value, key)?, key)?;
                config.network_smoothing = parse_smoothing(&text)?;
            }
            "theme" => {
                let text = as_string(cloned(value, key)?, key)?;
                config.theme = parse_theme(&text)?;
            }
            "history" => {
                let map = as_map(cloned(value, key)?, key)?;
                apply_history(&mut config.history, &map)?;
            }
            "alerts" => {
                let map = as_map(cloned(value, key)?, key)?;
                apply_alerts(&mut config.alerts, &map)?;
            }
            "overlay" => {
                let map = as_map(cloned(value, key)?, key)?;
                apply_overlay(&mut config.overlay, &map)?;
            }
            "plugins" => {
                let map = as_map(cloned(value, key)?, key)?;
                apply_plugins(&mut config.plugins, &map)?;
            }
            "process_traffic" => {
                let map = as_map(cloned(value, key)?, key)?;
                apply_process_traffic(&mut config.process_traffic, &map)?;
            }
            other => return Err(format!("unknown config key '{other}'")),
        }
    }
    Ok(())
}

fn apply_history(
    config: &mut HistoryConfig,
    map: &HashMap<String, OwnedValue>,
) -> Result<(), String> {
    for (key, value) in map {
        let path = format!("history.{key}");
        match key.as_str() {
            "retention_days" => config.retention_days = as_u32(cloned(value, &path)?, &path)?,
            "timezone" => config.timezone = as_string(cloned(value, &path)?, &path)?,
            other => return Err(format!("unknown config key 'history.{other}'")),
        }
    }
    Ok(())
}

fn apply_alerts(
    config: &mut AlertsConfig,
    map: &HashMap<String, OwnedValue>,
) -> Result<(), String> {
    for (key, value) in map {
        match key.as_str() {
            "rules" => {
                let entries = as_variants(cloned(value, "alerts.rules")?, "alerts.rules")?;
                let rules = entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| rule_from_dbus(entry, index))
                    .collect::<Result<Vec<AlertRule>, String>>()?;
                config.rules = rules;
            }
            other => return Err(format!("unknown config key 'alerts.{other}'")),
        }
    }
    Ok(())
}

fn rule_from_dbus(entry: &HashMap<String, OwnedValue>, index: usize) -> Result<AlertRule, String> {
    let mut rule = AlertRule::default();
    let path = format!("alerts.rules[{index}]");
    for (key, value) in entry {
        let field = format!("{path}.{key}");
        match key.as_str() {
            "enabled" => rule.enabled = as_bool(cloned(value, &field)?, &field)?,
            "metric" => rule.metric = as_string(cloned(value, &field)?, &field)?,
            "op" => rule.op = as_string(cloned(value, &field)?, &field)?,
            "threshold" => rule.threshold = as_f64(cloned(value, &field)?, &field)?,
            "duration_s" => rule.duration_s = as_u32(cloned(value, &field)?, &field)?,
            "cooldown_s" => rule.cooldown_s = as_u32(cloned(value, &field)?, &field)?,
            "quiet_hours" => rule.quiet_hours = as_string(cloned(value, &field)?, &field)?,
            "level" => rule.level = as_string(cloned(value, &field)?, &field)?,
            other => return Err(format!("unknown config key '{path}.{other}'")),
        }
    }
    Ok(rule)
}

fn apply_overlay(
    config: &mut OverlayConfig,
    map: &HashMap<String, OwnedValue>,
) -> Result<(), String> {
    for (key, value) in map {
        let path = format!("overlay.{key}");
        match key.as_str() {
            "visible" => config.visible = as_bool(cloned(value, &path)?, &path)?,
            "position" => config.position = as_position(cloned(value, &path)?, &path)?,
            "position_preset" => {
                let text = as_string(cloned(value, &path)?, &path)?;
                config.position_preset = parse_preset(&text)?;
            }
            "opacity" => config.opacity = as_f64(cloned(value, &path)?, &path)? as f32,
            "click_through" => config.click_through = as_bool(cloned(value, &path)?, &path)?,
            "always_on_top" => config.always_on_top = as_bool(cloned(value, &path)?, &path)?,
            "skin" => config.skin = as_string(cloned(value, &path)?, &path)?,
            "units" => {
                let text = as_string(cloned(value, &path)?, &path)?;
                config.units = parse_units(&text)?;
            }
            "items" => {
                let entries = as_variants(cloned(value, "overlay.items")?, "overlay.items")?;
                let items = entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| item_from_dbus(entry, index))
                    .collect::<Result<Vec<DisplayItem>, String>>()?;
                config.items = items;
            }
            other => return Err(format!("unknown config key 'overlay.{other}'")),
        }
    }
    Ok(())
}

fn item_from_dbus(
    entry: &HashMap<String, OwnedValue>,
    index: usize,
) -> Result<DisplayItem, String> {
    let mut item = DisplayItem::default();
    let path = format!("overlay.items[{index}]");
    for (key, value) in entry {
        let field = format!("{path}.{key}");
        match key.as_str() {
            "id" => item.id = as_string(cloned(value, &field)?, &field)?,
            "label" => item.label = as_string(cloned(value, &field)?, &field)?,
            "unit" => item.unit = as_string(cloned(value, &field)?, &field)?,
            "format" => item.format = as_string(cloned(value, &field)?, &field)?,
            "color" => item.color = as_string(cloned(value, &field)?, &field)?,
            "visible" => item.visible = as_bool(cloned(value, &field)?, &field)?,
            other => return Err(format!("unknown config key '{path}.{other}'")),
        }
    }
    Ok(item)
}

fn apply_plugins(
    config: &mut PluginsConfig,
    map: &HashMap<String, OwnedValue>,
) -> Result<(), String> {
    for (key, value) in map {
        let path = format!("plugins.{key}");
        match key.as_str() {
            "enabled" => config.enabled = as_bool(cloned(value, &path)?, &path)?,
            other => return Err(format!("unknown config key 'plugins.{other}'")),
        }
    }
    Ok(())
}

fn apply_process_traffic(
    config: &mut ProcessTrafficConfig,
    map: &HashMap<String, OwnedValue>,
) -> Result<(), String> {
    for (key, value) in map {
        let path = format!("process_traffic.{key}");
        match key.as_str() {
            "backend" => {
                let text = as_string(cloned(value, &path)?, &path)?;
                config.backend = parse_backend(&text)?;
            }
            other => return Err(format!("unknown config key 'process_traffic.{other}'")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        Snapshot::new(
            1,
            NetSample {
                iface: "eth0".into(),
                rx_bps: 1,
                tx_bps: 2,
                rx_total: 3,
                tx_total: 4,
            },
            CpuSample {
                total_pct: 5.0,
                per_core: vec![5.0],
                freq_mhz: None,
                load: None,
            },
            MemSample {
                used_pct: 6.0,
                total: 7,
                available: 8,
                swap_pct: 9.0,
            },
        )
    }

    #[test]
    fn optional_m4_keys_are_omitted_until_collected() {
        let dict = snapshot_to_dbus(&snapshot());
        for key in ["ts_ms", "net", "cpu", "mem", "temps", "plugin"] {
            assert!(dict.contains_key(key), "{key}");
        }
        for key in ["filesystems", "battery", "uptime_s"] {
            assert!(!dict.contains_key(key), "{key}");
        }
    }

    #[test]
    fn cpu_temp_is_optional_and_round_trips() {
        let mut snapshot = snapshot();
        assert!(!snapshot_to_dbus(&snapshot).contains_key("cpu_temp"));

        snapshot.cpu_temp = Some(66.5);
        let dict = snapshot_to_dbus(&snapshot);
        assert!(dict.contains_key("cpu_temp"));

        let decoded = snapshot_from_dbus(&dict).unwrap();
        assert_eq!(decoded.cpu_temp, Some(66.5));
    }

    #[test]
    fn gpu_alias_and_list_are_exported_when_present() {
        let mut snapshot = snapshot();
        let gpu = GpuSample {
            name: "Intel GPU".into(),
            util_pct: 12.0,
            mem_used: 0,
            mem_total: 0,
            freq_mhz: Some(900),
            temp_celsius: None,
            stale: true,
            observed_at_ms: Some(42),
        };
        snapshot.gpu = Some(gpu.clone());
        snapshot.gpus = vec![gpu];
        let dict = snapshot_to_dbus(&snapshot);
        assert!(dict.contains_key("gpu"));
        assert!(dict.contains_key("gpus"));
    }

    #[test]
    fn optional_m4_keys_are_added_without_changing_v1_keys() {
        let mut snapshot = snapshot();
        snapshot.filesystems = Some(vec![FilesystemSample {
            mount: "/".into(),
            total: 100,
            available: 40,
            used_pct: 60.0,
        }]);
        snapshot.battery = Some(BatterySample {
            percent: 50.0,
            status: "Charging".into(),
        });
        snapshot.uptime_s = Some(42);
        let dict = snapshot_to_dbus(&snapshot);
        for key in ["filesystems", "battery", "uptime_s"] {
            assert!(dict.contains_key(key), "{key}");
        }
    }

    #[test]
    fn config_round_trips_through_dbus_dict() {
        let config = Config::default();
        let dict = config_to_dbus(&config);
        let mut decoded = Config::default();
        apply_config_update(&mut decoded, &dict).unwrap();
        assert_eq!(decoded, config);
    }

    #[test]
    fn process_traffic_snapshot_key_is_optional() {
        let mut snapshot = snapshot();
        assert!(!snapshot_to_dbus(&snapshot).contains_key("processes"));
        snapshot.processes = Some(vec![ProcessTrafficSample {
            pid: 42,
            name: "curl".into(),
            rx_bps: 100,
            tx_bps: 200,
            rx_total: 1_000,
            tx_total: 2_000,
        }]);
        let dict = snapshot_to_dbus(&snapshot);
        assert_eq!(dict["processes"].value_signature().to_string(), "av");
        let decoded = snapshot_from_dbus(&dict).unwrap();
        assert_eq!(decoded.processes, snapshot.processes);
    }

    #[test]
    fn process_traffic_backend_round_trips_and_rejects_unknown_values() {
        let mut config = Config::default();
        let value = owned(process_traffic_value(&ProcessTrafficConfig {
            backend: ProcessTrafficBackend::Auto,
        }));
        apply_config_update(
            &mut config,
            &HashMap::from([("process_traffic".to_string(), value)]),
        )
        .unwrap();
        assert_eq!(config.process_traffic.backend, ProcessTrafficBackend::Auto);

        let mut map: HashMap<String, Value<'static>> = HashMap::new();
        map.insert("backend".into(), Value::from("bogus"));
        let error = apply_config_update(
            &mut config,
            &HashMap::from([("process_traffic".to_string(), owned(Value::from(map)))]),
        )
        .unwrap_err();
        assert!(error.contains("off, ebpf, af_packet or auto"), "{error}");
    }

    #[test]
    fn partial_config_update_merges_and_rejects_unknown_keys() {
        let mut config = Config::default();
        let updates: HashMap<String, OwnedValue> = HashMap::from([
            ("memory_include_cache".into(), owned(Value::from(true))),
            (
                "plugins".into(),
                owned(plugins_value(&PluginsConfig { enabled: false })),
            ),
        ]);
        apply_config_update(&mut config, &updates).unwrap();
        assert!(config.memory_include_cache);
        assert!(!config.plugins.enabled);
        assert_eq!(config.sample_interval_ms, 1000);
        let error = apply_config_update(
            &mut config,
            &HashMap::from([("unknown_key".to_string(), owned(Value::from(1u32)))]),
        )
        .unwrap_err();
        assert!(error.contains("unknown config key"));
    }
}
