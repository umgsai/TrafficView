//! daemon 调度：全接口采样、历史累计、自动选卡与 D-Bus 信号广播。

use std::{
    collections::{BTreeMap, HashMap},
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use chrono::Utc;
use tokio::sync::{watch, RwLock};
use tracing::{debug, info, warn};
use trafficview_collectors::{
    cpu::{read_freq_mhz, read_loadavg, ProcCpu},
    disk::{read_filesystems, DiskSampler},
    gpu::GpuSampler,
    hwmon::read_temps,
    mem::read_mem_with_mode,
    net::NetSampler,
    netlink::NetlinkWatcher,
    power::read_battery,
    thermal::{merge_thermal_temps, read_thermal_temps},
    uptime::UptimeState,
};
use trafficview_core::{
    clock::monotonic_ms,
    config::{Config, ConfigStore},
    i18n,
    smoothing::Smoother,
    temp::cpu_temperature,
    ColorScheme, CpuSample, InterfaceInfo, NetSample, Snapshot,
};
use zbus::object_server::SignalContext;

use crate::{
    alerts::{self, AlertEngine, Metric},
    dbus::{snapshot_to_dbus, Daemon1, BUS_NAME, PATH},
    history::HistoryStore,
    network::{self, NetSelector},
    plugins::{self, PluginSupervisor},
    process_traffic::{self, ProcessTrafficHandle},
};

pub use crate::network::{default_route_interface, parse_default_route};

/// 接口详情的兜底刷新周期；netlink 链路事件仍会立即触发刷新。
const INTERFACE_REFRESH: Duration = Duration::from_secs(30);

/// 采集器可用性；供设置界面区分“零值”与“不可用”。
#[derive(Debug, Clone, Default)]
pub struct CollectorStatus {
    pub available: bool,
    pub reason: String,
}

impl CollectorStatus {
    fn sampled<T>(value: &Option<T>, reason: &str) -> Self {
        Self {
            available: value.is_some(),
            reason: if value.is_some() {
                String::new()
            } else {
                reason.into()
            },
        }
    }
}

/// daemon 的唯一可变状态；D-Bus 接口与调度器共享。
pub struct SharedState {
    pub config: Config,
    pub snapshot: Option<Snapshot>,
    pub paused: bool,
    pub interfaces: Vec<String>,
    /// 含稳定键的网卡详情；仅由 daemon 更新。
    pub interface_details: Vec<InterfaceInfo>,
    /// 当前实际采样的稳定键（`mac:...` 或 `name:...`）。
    pub active_stable: Option<String>,
    /// 历史库不可用时为 `None`，daemon 继续提供实时指标。
    pub history: Option<Arc<Mutex<HistoryStore>>>,
    /// 告警状态机；配置 reload 时重建规则。
    pub alerts: Mutex<AlertEngine>,
    /// portal 最近一次报告的原始配色值（0/1/2）；不可用时为 `None`。
    pub portal_scheme: Option<u32>,
    /// 供前端读取的最终配色。
    pub color_scheme: ColorScheme,
    /// 最近一帧各采集器的可用性。
    pub collector_status: BTreeMap<String, CollectorStatus>,
    /// 子进程插件监督器；`None` 表示该入口不加载插件。
    pub plugin_supervisor: Option<Arc<PluginSupervisor>>,
    /// 可选进程流量消费者；默认 backend=off 时只发布关闭原因。
    pub process_traffic: Option<Arc<ProcessTrafficHandle>>,
    pub shutdown: watch::Sender<bool>,
}

/// 枚举 `/sys/class/net` 下的网卡名（排序后保证菜单稳定）。
pub fn list_interfaces(net_root: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(net_root)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// 启动 daemon：注册名称、启动调度、等待 SIGINT 或 Quit。
pub async fn run(config_store: ConfigStore) -> anyhow::Result<()> {
    run_with_options(config_store, None, plugins::default_root()).await
}

/// 可注入历史库路径（测试使用临时目录，避免污染真实 XDG data）。
pub async fn run_with_history(
    config_store: ConfigStore,
    history_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    run_with_options(config_store, history_path, None).await
}

/// 可注入历史库与插件目录；`plugin_root=None` 时不加载任何插件。
pub async fn run_with_options(
    config_store: ConfigStore,
    history_path: Option<PathBuf>,
    plugin_root: Option<PathBuf>,
) -> anyhow::Result<()> {
    let config = config_store
        .load_or_create()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let history = open_history(history_path, &config);
    let mut alert_engine = AlertEngine::new(config.alerts.rules.clone());
    alert_engine.set_language(i18n::resolve(&config.language));
    let alerts = Mutex::new(alert_engine);
    let plugin_supervisor = plugin_root.map(|root| {
        Arc::new(PluginSupervisor::start(
            root,
            plugins::RuntimeConfig::default(),
            config.plugins.enabled,
        ))
    });
    let process_backend = config.process_traffic.backend;
    let process_interval = Duration::from_millis(u64::from(config.sample_interval_ms.max(1000)));
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let process_traffic = Arc::new(process_traffic::spawn(
        process_backend,
        process_interval,
        shutdown_tx.subscribe(),
    ));
    let state = Arc::new(RwLock::new(SharedState {
        config,
        snapshot: None,
        paused: false,
        interfaces: list_interfaces(Path::new("/sys/class/net")),
        interface_details: Vec::new(),
        active_stable: None,
        history,
        alerts,
        portal_scheme: None,
        color_scheme: ColorScheme::Dark,
        collector_status: BTreeMap::new(),
        plugin_supervisor: plugin_supervisor.clone(),
        process_traffic: Some(process_traffic.clone()),
        shutdown: shutdown_tx,
    }));
    let daemon = Daemon1::new(state.clone(), config_store);
    let connection = match zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(PATH, daemon)?
        .build()
        .await
    {
        Ok(connection) => connection,
        Err(zbus::Error::NameTaken) => {
            info!("{BUS_NAME} is already owned; not starting a second sampler");
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let context: SignalContext<'static> = SignalContext::new(&connection, PATH)?;
    let interfaces = state.read().await.interfaces.clone();
    let _ = Daemon1::notify_interfaces(&context, interfaces).await;

    // portal 读取可能触发服务激活甚至超时；放到后台，绝不阻塞采样。
    tokio::spawn(theme_init(
        state.clone(),
        context.clone(),
        connection.clone(),
    ));

    let task = tokio::spawn(scheduler(
        state.clone(),
        context,
        PathBuf::from("/proc"),
        PathBuf::from("/sys/class/net"),
    ));

    tokio::select! {
        _ = tokio::signal::ctrl_c() => info!("ctrl-c received, shutting down"),
        _ = shutdown_rx.changed() => info!("quit requested over D-Bus"),
    }
    let _ = state.read().await.shutdown.send(true);
    let _ = task.await;
    if let Some(supervisor) = plugin_supervisor {
        supervisor.stop();
    }
    process_traffic.stop();
    Ok(())
}

fn open_history(explicit: Option<PathBuf>, config: &Config) -> Option<Arc<Mutex<HistoryStore>>> {
    let path = explicit.or_else(HistoryStore::default_path)?;
    match HistoryStore::open(
        &path,
        &config.history.timezone,
        config.history.retention_days,
    ) {
        Ok(store) => {
            info!(path = %path.display(), "traffic history is ready");
            Some(Arc::new(Mutex::new(store)))
        }
        Err(error) => {
            warn!(%error, path = %path.display(), "cannot open traffic history; continuing without it");
            None
        }
    }
}

struct RuntimeSampler {
    net: NetSampler,
    cpu: ProcCpu,
    disk: DiskSampler,
    gpu: GpuSampler,
    uptime: UptimeState,
    last: Instant,
    smoothing: Smoother,
    smoothed_iface: Option<String>,
    watcher: Option<NetlinkWatcher>,
}

impl RuntimeSampler {
    fn new(proc_root: &Path, config: &Config) -> io::Result<Self> {
        let mut net = NetSampler::new(proc_root);
        let mut cpu = ProcCpu::new(proc_root);
        let mut disk = DiskSampler::new(proc_root);
        // 建立首帧基线；速率在下一次采样才有效。
        let _ = net.sample_all(Duration::ZERO)?;
        let _ = cpu.sample(Duration::ZERO)?;
        let _ = disk.sample(Duration::ZERO);
        let watcher = match NetlinkWatcher::spawn() {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                warn!(%error, "netlink hotplug watcher unavailable; polling each sample");
                None
            }
        };
        Ok(Self {
            net,
            cpu,
            disk,
            gpu: GpuSampler::new("/sys/class/drm", proc_root),
            uptime: UptimeState::default(),
            last: Instant::now(),
            smoothing: Smoother::new(config.network_smoothing),
            smoothed_iface: None,
            watcher,
        })
    }
}

async fn scheduler(
    state: Arc<RwLock<SharedState>>,
    context: SignalContext<'static>,
    proc_root: PathBuf,
    sys_root: PathBuf,
) {
    let mut shutdown = state.read().await.shutdown.subscribe();
    let supervisor = state.read().await.plugin_supervisor.clone();
    let process_traffic = state.read().await.process_traffic.clone();
    let mut sampler: Option<RuntimeSampler> = None;
    // 接口详情（MAC/状态/carrier）不需要每秒重扫 sysfs：netlink 报告链路变化时立即刷新，
    // 否则按固定周期兜底，避免把采样预算耗在成百上千次小文件读取上。
    let mut interfaces: Vec<InterfaceInfo> = Vec::new();
    let mut interfaces_at: Option<Instant> = None;
    loop {
        let (
            interval_ms,
            paused,
            configured_iface,
            include_cache,
            whitelist,
            blacklist,
            smoothing,
            cpu_temp_sensor,
        ) = {
            let state = state.read().await;
            (
                state.config.sample_interval_ms,
                state.paused,
                state.config.active_interface.clone(),
                state.config.memory_include_cache,
                state.config.interface_whitelist.clone(),
                state.config.interface_blacklist.clone(),
                state.config.network_smoothing,
                state.config.overlay.cpu_temp_sensor.clone(),
            )
        };
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(interval_ms as u64)) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
        if *shutdown.borrow() {
            break;
        }
        if paused {
            // 恢复采样时重建基线，避免把暂停时长算进速率。
            sampler = None;
            continue;
        }
        if sampler.is_none() {
            let config = state.read().await.config.clone();
            match RuntimeSampler::new(&proc_root, &config) {
                Ok(active) => {
                    info!(
                        source = active.net.last_source(),
                        "network counter baseline established"
                    );
                    sampler = Some(active);
                }
                Err(error) => {
                    warn!(%error, "cannot establish a collector baseline");
                    continue;
                }
            }
        }
        let Some(active) = sampler.as_mut() else {
            continue;
        };
        if active.smoothing.kind() != smoothing {
            active.smoothing = Smoother::new(smoothing);
            active.smoothed_iface = None;
        }
        let link_changed = active
            .watcher
            .as_ref()
            .is_some_and(NetlinkWatcher::take_dirty);
        if link_changed {
            debug!("netlink reported a link/route change; refreshing interfaces now");
        }

        let now = Instant::now();
        let elapsed = now.duration_since(active.last);
        active.last = now;
        let counters = match active.net.sample_all(elapsed) {
            Ok(counters) => counters,
            Err(error) => {
                warn!(%error, "network sampling failed; daemon remains alive");
                continue;
            }
        };
        let cpu = match active.cpu.sample(elapsed) {
            Ok(Some(cpu)) => cpu,
            Ok(None) => {
                debug!("discarding a CPU baseline-only frame");
                continue;
            }
            Err(error) => {
                warn!(%error, "CPU sampling failed; daemon remains alive");
                continue;
            }
        };
        let mem = match read_mem_with_mode(&proc_root, include_cache) {
            Ok(mem) => mem,
            Err(error) => {
                warn!(%error, "memory sampling failed; daemon remains alive");
                continue;
            }
        };

        let sys_class = sys_root.parent().unwrap_or(Path::new("/sys/class"));
        let disk = match active.disk.sample(elapsed) {
            Ok(disk) => disk,
            Err(error) => {
                warn!(%error, "disk sampling failed; keeping other metrics");
                None
            }
        };
        let temps = merge_thermal_temps(
            read_temps(&sys_class.join("hwmon")),
            read_thermal_temps(&sys_class.join("thermal")),
        );
        let battery = read_battery(&sys_class.join("power_supply"));
        let filesystems = read_filesystems(&["/".to_string()]);
        let uptime_s = active.uptime.read(&proc_root);
        let freq_mhz = read_freq_mhz(Path::new("/sys/devices/system/cpu"));
        let load = read_loadavg(&proc_root);
        let (gpus, gpu_reason) = active.gpu.sample(Instant::now(), monotonic_ms());
        let gpu = gpus.first().cloned();
        let mut collector_status = BTreeMap::from([
            (
                "gpu".to_string(),
                CollectorStatus {
                    available: gpu_reason.is_none(),
                    reason: gpu_reason.unwrap_or_default(),
                },
            ),
            (
                "disk".to_string(),
                CollectorStatus::sampled(&disk, "no whole-disk counters"),
            ),
            (
                "temps".to_string(),
                CollectorStatus {
                    available: !temps.is_empty(),
                    reason: if temps.is_empty() {
                        "no hwmon or thermal sensors".into()
                    } else {
                        String::new()
                    },
                },
            ),
            (
                "battery".to_string(),
                CollectorStatus::sampled(&battery, "no battery"),
            ),
            (
                "filesystems".to_string(),
                CollectorStatus::sampled(&filesystems, "no readable mount point"),
            ),
            (
                "uptime".to_string(),
                CollectorStatus::sampled(&uptime_s, "uptime unreadable or went backwards"),
            ),
            (
                "cpufreq".to_string(),
                CollectorStatus::sampled(&freq_mhz, "cpufreq unavailable"),
            ),
            (
                "loadavg".to_string(),
                CollectorStatus::sampled(&load, "loadavg unavailable"),
            ),
        ]);
        if let Some(supervisor) = &supervisor {
            for (id, status) in supervisor.statuses() {
                collector_status.insert(
                    format!("plugin.{id}"),
                    CollectorStatus {
                        available: status.available,
                        reason: status.reason,
                    },
                );
            }
        }
        if let Some(monitor) = &process_traffic {
            let status = monitor.status();
            collector_status.insert(
                "processes".to_string(),
                CollectorStatus {
                    available: status.available,
                    reason: status.reason,
                },
            );
        }

        let stale = interfaces_at.is_none_or(|at| at.elapsed() >= INTERFACE_REFRESH);
        if interfaces.is_empty() || link_changed || stale {
            interfaces = network::discover(&sys_root);
            interfaces_at = Some(Instant::now());
        }
        let details = interfaces.clone();
        refresh_interfaces(&state, &context, &details).await;
        record_history(&state, &details, &counters).await;

        let rates: HashMap<String, u64> = counters
            .iter()
            .map(|(name, sample)| {
                (
                    name.clone(),
                    sample.rx_bps.unwrap_or(0) + sample.tx_bps.unwrap_or(0),
                )
            })
            .collect();
        let chosen = if configured_iface.is_empty() {
            let default_route = default_route_interface(&proc_root).ok();
            match NetSelector::new(&whitelist, &blacklist, default_route) {
                Ok(selector) => selector
                    .choose(&details, &rates)
                    .ok()
                    .map(|iface| iface.name.clone()),
                Err(error) => {
                    warn!(%error, "invalid interface patterns in configuration");
                    None
                }
            }
        } else {
            details
                .iter()
                .find(|iface| iface.name == configured_iface)
                .map(|iface| iface.name.clone())
                .or(Some(configured_iface))
        };
        let Some(chosen) = chosen else {
            warn!("no active interface; keeping the daemon and history available");
            continue;
        };
        let Some(sample) = counters.get(&chosen) else {
            warn!(iface = %chosen, "chosen interface has no counters in this frame");
            continue;
        };
        let (Some(raw_rx), Some(raw_tx)) = (sample.rx_bps, sample.tx_bps) else {
            debug!(iface = %chosen, "discarding a network baseline-only frame");
            continue;
        };
        if active.smoothed_iface.as_deref() != Some(chosen.as_str()) {
            active.smoothing.reset();
            active.smoothed_iface = Some(chosen.clone());
        }
        let (rx_bps, tx_bps) = active.smoothing.apply(raw_rx, raw_tx);
        evaluate_alerts(&state, &context, cpu.0, mem.used_pct, raw_rx, raw_tx).await;
        let stable = details
            .iter()
            .find(|iface| iface.name == chosen)
            .map(|iface| iface.stable.clone())
            .unwrap_or_else(|| network::stable_key(&chosen, None));
        let mut snapshot = Snapshot::new(
            monotonic_ms(),
            NetSample {
                iface: chosen.clone(),
                rx_bps,
                tx_bps,
                rx_total: sample.rx_total,
                tx_total: sample.tx_total,
            },
            CpuSample {
                total_pct: cpu.0,
                per_core: cpu.1,
                freq_mhz,
                load,
            },
            mem,
        );
        snapshot.disk = disk;
        snapshot.temps = temps;
        snapshot.cpu_temp = cpu_temperature(&snapshot.temps, Some(cpu_temp_sensor.as_str()));
        snapshot.filesystems = filesystems;
        snapshot.battery = battery;
        snapshot.uptime_s = uptime_s;
        snapshot.gpu = gpu;
        snapshot.gpus = gpus;
        snapshot.processes = process_traffic
            .as_ref()
            .and_then(|monitor| monitor.processes());
        if let Some(supervisor) = &supervisor {
            snapshot.plugin = supervisor.published_values();
        }
        let dict = snapshot_to_dbus(&snapshot);
        {
            let mut state = state.write().await;
            let changed = state.active_stable.as_deref() != Some(stable.as_str());
            state.snapshot = Some(snapshot);
            state.active_stable = Some(stable.clone());
            state.collector_status = collector_status;
            drop(state);
            if changed {
                info!(iface = %chosen, %stable, "active network interface changed");
            }
        }
        if let Err(error) = Daemon1::sample(&context, dict).await {
            warn!(%error, "failed to emit Sample signal");
        }
    }
}

async fn theme_init(
    state: Arc<RwLock<SharedState>>,
    context: SignalContext<'static>,
    connection: zbus::Connection,
) {
    let value = match tokio::time::timeout(
        Duration::from_millis(500),
        crate::theme::read_portal_scheme(&connection),
    )
    .await
    {
        Ok(value) => value,
        Err(_) => {
            warn!("appearance portal did not answer within 500ms; keeping dark theme");
            None
        }
    };
    if let Some(value) = value {
        let scheme = {
            let mut state = state.write().await;
            state.portal_scheme = Some(value);
            state.color_scheme = crate::theme::decide(state.config.theme, Some(value));
            state.color_scheme
        };
        info!(
            portal = value,
            scheme = crate::theme::scheme_name(scheme),
            "desktop color scheme detected"
        );
    }
    if let Some(receiver) = crate::theme::subscribe(&connection).await {
        theme_watcher(state, context, receiver).await;
    }
}

async fn theme_watcher(
    state: Arc<RwLock<SharedState>>,
    context: SignalContext<'static>,
    mut receiver: tokio::sync::mpsc::Receiver<u32>,
) {
    while let Some(value) = receiver.recv().await {
        let scheme = {
            let mut state = state.write().await;
            state.portal_scheme = Some(value);
            state.color_scheme = crate::theme::decide(state.config.theme, Some(value));
            state.color_scheme
        };
        info!(
            portal = value,
            scheme = crate::theme::scheme_name(scheme),
            "desktop color scheme changed"
        );
        let _ = Daemon1::config_changed(&context).await;
    }
}

async fn refresh_interfaces(
    state: &Arc<RwLock<SharedState>>,
    context: &SignalContext<'static>,
    details: &[InterfaceInfo],
) {
    let names: Vec<String> = details.iter().map(|iface| iface.name.clone()).collect();
    let mut state = state.write().await;
    let changed = state.interfaces != names;
    state.interfaces = names.clone();
    state.interface_details = details.to_vec();
    drop(state);
    if changed {
        info!(?names, "network interface set changed");
        let _ = Daemon1::notify_interfaces(context, names).await;
    }
}

async fn evaluate_alerts(
    state: &Arc<RwLock<SharedState>>,
    context: &SignalContext<'static>,
    cpu_pct: f32,
    mem_pct: f32,
    rx_bps: u64,
    tx_bps: u64,
) {
    let now_secs = Utc::now().timestamp();
    let (month_enabled, history, lang) = {
        let state = state.read().await;
        (
            state
                .config
                .alerts
                .rules
                .iter()
                .any(|rule| rule.enabled && rule.metric == "month_bytes"),
            state.history.clone(),
            i18n::resolve(&state.config.language),
        )
    };
    let month_bytes = if month_enabled {
        history.and_then(|history| {
            let store = history.lock().ok()?;
            let summary = store.summary("month", None, Utc::now()).ok()?;
            Some(summary.rx + summary.tx)
        })
    } else {
        None
    };
    let events = {
        let state = state.read().await;
        let Ok(mut engine) = state.alerts.lock() else {
            warn!("alert engine mutex is poisoned");
            return;
        };
        let mut events = engine.observe_cpu(now_secs, f64::from(cpu_pct));
        events.extend(engine.observe_mem(now_secs, f64::from(mem_pct)));
        events.extend(engine.observe(Metric::NetRxBps, now_secs, rx_bps as f64));
        events.extend(engine.observe(Metric::NetTxBps, now_secs, tx_bps as f64));
        if let Some(bytes) = month_bytes {
            events.extend(engine.observe(Metric::MonthBytes, now_secs, bytes as f64));
        }
        events
    };
    for event in events {
        info!(
            code = %event.code,
            active = event.active,
            "alert event: {}",
            event.message
        );
        if let Err(error) = Daemon1::alert(
            context,
            event.level.clone(),
            event.code.clone(),
            event.message.clone(),
        )
        .await
        {
            warn!(%error, "failed to emit Alert signal");
        }
        if event.active {
            let desktop = event.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(error) = alerts::send_desktop_notification(&desktop, lang) {
                    warn!(%error, "desktop notification is unavailable");
                }
            });
        }
    }
}

async fn record_history(
    state: &Arc<RwLock<SharedState>>,
    details: &[InterfaceInfo],
    counters: &std::collections::BTreeMap<String, trafficview_collectors::net::InterfaceSample>,
) {
    let Some(history) = state.read().await.history.clone() else {
        return;
    };
    let stable: HashMap<&str, &str> = details
        .iter()
        .map(|iface| (iface.name.as_str(), iface.stable.as_str()))
        .collect();
    let now = Utc::now();
    let Ok(mut store) = history.lock() else {
        warn!("history mutex is poisoned; skipping this sample");
        return;
    };
    for (name, sample) in counters {
        let stable = stable
            .get(name.as_str())
            .copied()
            .map(str::to_owned)
            .unwrap_or_else(|| network::stable_key(name, None));
        let result = store
            .upsert_interface(&stable, name)
            .and_then(|id| store.observe(id, now, sample.rx_total, sample.tx_total));
        if let Err(error) = result {
            warn!(%error, iface = %name, "cannot persist traffic history");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUTE: &str = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\nenp0s3\t00000000\t020010AC\t0003\t0\t0\t100\t00000000\t0\t0\t0\n";

    #[test]
    fn parses_default_route() {
        assert_eq!(parse_default_route(ROUTE).as_deref(), Some("enp0s3"));
        assert_eq!(parse_default_route(""), None);
    }

    #[test]
    fn ignores_non_default_routes() {
        let route = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\neth0\t000010AC\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(parse_default_route(route), None);
    }
}
