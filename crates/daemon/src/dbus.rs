//! D-Bus v1 冻结契约（M1）。
//!
//! BUS/PATH/IFACE 均为 `com.trafficview.Daemon1`（session bus）。

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use chrono::Utc;
use tokio::sync::RwLock;
use tracing::warn;
use trafficview_core::{
    config::{ConfigStore, SAMPLE_INTERVAL_MAX_MS, SAMPLE_INTERVAL_MIN_MS},
    CpuSample, DiskSample, GpuSample, InterfaceInfo, MemSample, NetSample, Snapshot, TempSample,
    TrafficSummary,
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
        {
            let mut state = self.state.write().await;
            state.config = config;
            state.color_scheme = crate::theme::decide(state.config.theme, state.portal_scheme);
            if let Ok(mut engine) = state.alerts.lock() {
                engine.reload(&rules);
            };
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
    top.insert("plugin".into(), owned(plugin_value(&snapshot.plugin)));
    top
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
            Ok(GpuSample {
                name: string(&gpu, "name")?,
                util_pct: f64::try_from(required(&gpu, "util_pct")?.try_clone()?)? as f32,
                mem_used: u64::try_from(required(&gpu, "mem_used")?)?,
                mem_total: u64::try_from(required(&gpu, "mem_total")?)?,
            })
        })
        .transpose()?;

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
    let plugin = dict
        .get("plugin")
        .map(|value| -> anyhow::Result<BTreeMap<String, String>> {
            let map: HashMap<String, String> = value.try_clone()?.try_into()?;
            Ok(map.into_iter().collect())
        })
        .transpose()?
        .unwrap_or_default();

    Ok(Snapshot {
        ts_ms,
        net,
        cpu,
        mem,
        disk,
        gpu,
        temps,
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
