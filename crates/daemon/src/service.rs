//! daemon 调度：单采集器、固定周期、D-Bus 信号广播。

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::{watch, RwLock};
use tracing::{debug, info, warn};
use trafficview_collectors::{cpu::ProcCpu, mem::read_mem_with_mode, net::ProcNetDev};
use trafficview_core::{
    clock::monotonic_ms,
    config::{Config, ConfigStore},
    CpuSample, Snapshot,
};
use zbus::object_server::SignalContext;

use crate::dbus::{snapshot_to_dbus, Daemon1, BUS_NAME, PATH};

/// daemon 的唯一可变状态；D-Bus 接口与调度器共享。
pub struct SharedState {
    pub config: Config,
    pub snapshot: Option<Snapshot>,
    pub paused: bool,
    pub interfaces: Vec<String>,
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

/// 从 `/proc/net/route` 文本解析默认路由网卡（Destination 与 Mask 均为 0 且 RTF_UP）。
pub fn parse_default_route(text: &str) -> Option<String> {
    text.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 {
            return None;
        }
        let flags = u16::from_str_radix(fields[3], 16).ok()?;
        let is_default = fields[1] == "00000000" && fields[7] == "00000000";
        (is_default && flags & 0x1 != 0).then(|| fields[0].to_owned())
    })
}

pub fn default_route_interface(proc_root: &Path) -> io::Result<String> {
    let text = fs::read_to_string(proc_root.join("net/route"))?;
    parse_default_route(&text)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no default route interface found"))
}

struct Sampler {
    iface: String,
    net: ProcNetDev,
    cpu: ProcCpu,
    last: Instant,
}

impl Sampler {
    fn new(iface: &str, proc_root: &Path) -> io::Result<Self> {
        let mut net = ProcNetDev::new(proc_root);
        let mut cpu = ProcCpu::new(proc_root);
        net.sample(iface, Duration::ZERO)?;
        cpu.sample(Duration::ZERO)?;
        Ok(Self {
            iface: iface.to_owned(),
            net,
            cpu,
            last: Instant::now(),
        })
    }

    fn sample(&mut self, proc_root: &Path, include_cache: bool) -> io::Result<Option<Snapshot>> {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last);
        self.last = now;
        let Some(net) = self.net.sample(&self.iface, elapsed)? else {
            return Ok(None);
        };
        let Some((total_pct, per_core)) = self.cpu.sample(elapsed)? else {
            return Ok(None);
        };
        let mem = read_mem_with_mode(proc_root, include_cache)?;
        Ok(Some(Snapshot::new(
            monotonic_ms(),
            net,
            CpuSample {
                total_pct,
                per_core,
                freq_mhz: None,
                load: None,
            },
            mem,
        )))
    }
}

/// 启动 daemon：注册名称、启动调度、等待 SIGINT 或 Quit。
pub async fn run(config_store: ConfigStore) -> anyhow::Result<()> {
    let config = config_store
        .load_or_create()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let state = Arc::new(RwLock::new(SharedState {
        config,
        snapshot: None,
        paused: false,
        interfaces: list_interfaces(Path::new("/sys/class/net")),
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

    let task = tokio::spawn(scheduler(state.clone(), context, PathBuf::from("/proc")));

    tokio::select! {
        _ = tokio::signal::ctrl_c() => info!("ctrl-c received, shutting down"),
        _ = shutdown_rx.changed() => info!("quit requested over D-Bus"),
    }
    let _ = state.read().await.shutdown.send(true);
    let _ = task.await;
    Ok(())
}

async fn scheduler(
    state: Arc<RwLock<SharedState>>,
    context: SignalContext<'static>,
    proc_root: PathBuf,
) {
    let mut shutdown = state.read().await.shutdown.subscribe();
    let mut sampler: Option<Sampler> = None;
    loop {
        let (interval_ms, paused, configured_iface, include_cache) = {
            let state = state.read().await;
            (
                state.config.sample_interval_ms,
                state.paused,
                state.config.active_interface.clone(),
                state.config.memory_include_cache,
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
        let iface = if configured_iface.is_empty() {
            match default_route_interface(&proc_root) {
                Ok(iface) => iface,
                Err(error) => {
                    warn!(%error, "cannot resolve the default route interface");
                    sampler = None;
                    continue;
                }
            }
        } else {
            configured_iface
        };
        let needs_baseline = sampler
            .as_ref()
            .map(|active| active.iface != iface)
            .unwrap_or(true);
        if needs_baseline {
            match Sampler::new(&iface, &proc_root) {
                Ok(active) => sampler = Some(active),
                Err(error) => {
                    warn!(%error, iface, "cannot establish a collector baseline");
                    sampler = None;
                }
            }
            continue;
        }
        let Some(active) = sampler.as_mut() else {
            continue;
        };
        match active.sample(&proc_root, include_cache) {
            Ok(Some(snapshot)) => {
                let dict = snapshot_to_dbus(&snapshot);
                state.write().await.snapshot = Some(snapshot);
                if let Err(error) = Daemon1::sample(&context, dict).await {
                    warn!(%error, "failed to emit Sample signal");
                }
            }
            Ok(None) => debug!("discarding a baseline-only frame"),
            Err(error) => warn!(%error, "sample failed; daemon remains alive"),
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
