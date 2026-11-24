use std::{
    collections::{BTreeMap, HashMap},
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};
use trafficview_core::{format::counter_rate, NetSample};

pub fn parse_proc_net_dev(input: &str, iface: &str) -> Option<(u64, u64)> {
    input.lines().skip(2).find_map(|line| {
        let (name, values) = line.split_once(':')?;
        if name.trim() != iface {
            return None;
        }
        let fields: Vec<&str> = values.split_whitespace().collect();
        Some((fields.first()?.parse().ok()?, fields.get(8)?.parse().ok()?))
    })
}

/// 解析全部 `/proc/net/dev` 行；一次读取即可回退覆盖所有接口。
pub fn parse_proc_net_dev_all(input: &str) -> Vec<(String, u64, u64)> {
    input
        .lines()
        .skip(2)
        .filter_map(|line| {
            let (name, values) = line.split_once(':')?;
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            let fields: Vec<&str> = values.split_whitespace().collect();
            Some((
                name.to_owned(),
                fields.first()?.parse().ok()?,
                fields.get(8)?.parse().ok()?,
            ))
        })
        .collect()
}

/// 一个接口在当前采样周期的原始计数器与速率；首帧速率为 `None`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceSample {
    pub rx_total: u64,
    pub tx_total: u64,
    pub rx_bps: Option<u64>,
    pub tx_bps: Option<u64>,
}

#[derive(Default)]
pub struct NetState {
    previous: Option<(u64, u64)>,
}

impl NetState {
    pub fn update(&mut self, current: (u64, u64), elapsed: Duration) -> Option<(u64, u64)> {
        let previous = self.previous.replace(current);
        let (rx, tx) = previous?;
        Some((
            counter_rate(rx, current.0, elapsed)?,
            counter_rate(tx, current.1, elapsed)?,
        ))
    }
}

pub struct ProcNetDev {
    root: PathBuf,
    state: NetState,
}

impl ProcNetDev {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            state: NetState::default(),
        }
    }

    pub fn sample(&mut self, iface: &str, elapsed: Duration) -> io::Result<Option<NetSample>> {
        let text = fs::read_to_string(self.root.join("net/dev"))?;
        let totals = parse_proc_net_dev(&text, iface).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("interface {iface} not found"),
            )
        })?;
        Ok(self
            .state
            .update(totals, elapsed)
            .map(|(rx_bps, tx_bps)| NetSample {
                iface: iface.to_owned(),
                rx_bps,
                tx_bps,
                rx_total: totals.0,
                tx_total: totals.1,
            }))
    }
}

impl Default for ProcNetDev {
    fn default() -> Self {
        Self::new(Path::new("/proc"))
    }
}

/// 全接口采样器：优先 netlink `IFLA_STATS64`，失败时一次解析全部 `/proc/net/dev`。
pub struct NetSampler {
    root: PathBuf,
    states: HashMap<String, NetState>,
    last_source: &'static str,
}

impl NetSampler {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            states: HashMap::new(),
            last_source: "none",
        }
    }

    /// 最近一次采样使用的数据源：`netlink`、`procfs` 或 `none`。
    pub fn last_source(&self) -> &'static str {
        self.last_source
    }

    pub fn sample_all(
        &mut self,
        elapsed: Duration,
    ) -> io::Result<BTreeMap<String, InterfaceSample>> {
        match crate::netlink::read_link_counters() {
            Ok(links) => {
                // 某些驱动/内核组合下，rtnetlink RTM_GETLINK dump 会漏掉接口
                // （例如某些 WiFi 驱动不暴露 IFLA_STATS64）。对比 /proc/net/dev
                // 确认没有漏网之鱼；漏了就回退到 procfs。
                let nl_names: std::collections::HashSet<_> =
                    links.iter().map(|l| l.name.as_str()).collect();
                let proc_text = fs::read_to_string(self.root.join("net/dev"))?;
                let proc_names: std::collections::HashSet<_> = parse_proc_net_dev_all(&proc_text)
                    .into_iter()
                    .map(|(name, _, _)| name)
                    .collect();
                let missing: Vec<_> =
                    proc_names.iter().filter(|n| !nl_names.contains(n.as_str())).collect();
                if !missing.is_empty() {
                    self.last_source = "procfs";
                    return self.sample_all_from_proc(elapsed);
                }
                self.last_source = "netlink";
                let counters: Vec<(String, u64, u64)> = links
                    .into_iter()
                    .map(|link| (link.name, link.rx_total, link.tx_total))
                    .collect();
                Ok(self.update(counters, elapsed))
            }
            Err(_) => {
                self.last_source = "procfs";
                self.sample_all_from_proc(elapsed)
            }
        }
    }

    /// 仅使用 procfs 的路径；netlink 不可用或缺少 `IFLA_STATS64` 时调用。
    pub fn sample_all_from_proc(
        &mut self,
        elapsed: Duration,
    ) -> io::Result<BTreeMap<String, InterfaceSample>> {
        let text = fs::read_to_string(self.root.join("net/dev"))?;
        let counters = parse_proc_net_dev_all(&text);
        if counters.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no interfaces found in /proc/net/dev",
            ));
        }
        Ok(self.update(counters, elapsed))
    }

    fn update(
        &mut self,
        counters: Vec<(String, u64, u64)>,
        elapsed: Duration,
    ) -> BTreeMap<String, InterfaceSample> {
        let mut samples = BTreeMap::new();
        for (name, rx_total, tx_total) in counters {
            let state = self.states.entry(name.clone()).or_default();
            let rate = state.update((rx_total, tx_total), elapsed);
            samples.insert(
                name,
                InterfaceSample {
                    rx_total,
                    tx_total,
                    rx_bps: rate.map(|value| value.0),
                    tx_bps: rate.map(|value| value.1),
                },
            );
        }
        self.states.retain(|name, _| samples.contains_key(name));
        samples
    }
}

impl Default for NetSampler {
    fn default() -> Self {
        Self::new(Path::new("/proc"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "Inter-| Receive | Transmit\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\n eth0: 1000 1 0 0 0 0 0 0 2000 2 0 0 0 0 0 0\n";

    #[test]
    fn parses_selected_interface() {
        assert_eq!(parse_proc_net_dev(FIXTURE, "eth0"), Some((1000, 2000)));
        assert_eq!(parse_proc_net_dev(FIXTURE, "wlan0"), None);
    }

    #[test]
    fn parses_all_proc_interfaces_in_one_pass() {
        let all = parse_proc_net_dev_all(FIXTURE);
        assert_eq!(all, vec![("eth0".to_owned(), 1000, 2000)]);
    }

    #[test]
    fn proc_sampler_tracks_each_interface_independently() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("net")).unwrap();
        let write = |rx: u64, tx: u64| {
            std::fs::write(
                directory.path().join("net/dev"),
                format!("Inter-| Receive | Transmit\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\n eth0: {rx} 1 0 0 0 0 0 0 {tx} 2 0 0 0 0 0 0\n wlan0: 5 1 0 0 0 0 0 0 6 2 0 0 0 0 0 0\n"),
            )
            .unwrap();
        };
        write(1000, 2000);
        let mut sampler = NetSampler::new(directory.path());
        let first = sampler
            .sample_all_from_proc(Duration::from_secs(1))
            .unwrap();
        assert_eq!(first["eth0"].rx_bps, None);
        write(2024, 4048);
        let second = sampler
            .sample_all_from_proc(Duration::from_secs(1))
            .unwrap();
        assert_eq!(second["eth0"].rx_bps, Some(1024));
        assert_eq!(second["eth0"].tx_bps, Some(2048));
        assert_eq!(
            second["wlan0"].rx_bps,
            Some(0),
            "unchanged counters yield zero rate"
        );
        assert_eq!(sampler.last_source(), "none");
    }

    #[test]
    fn first_sample_establishes_baseline() {
        let mut state = NetState::default();
        assert_eq!(state.update((100, 200), Duration::from_secs(1)), None);
        let second = state.update((1128, 2504), Duration::from_secs(1)).unwrap();
        assert_eq!((second.0, second.1), (1028, 2304));
    }

    #[test]
    fn reset_and_resume_only_rebuild_baseline() {
        let mut state = NetState::default();
        state.update((1000, 2000), Duration::from_secs(1));
        assert_eq!(state.update((10, 20), Duration::from_secs(1)), None);
        assert!(state.update((1034, 2100), Duration::from_secs(1)).is_some());
    }

    #[test]
    fn long_gap_discards_frame_and_rebuilds_baseline() {
        let mut state = NetState::default();
        state.update((100, 200), Duration::from_secs(1));
        assert_eq!(
            state.update((100_000, 200_000), Duration::from_secs(6)),
            None
        );
        let resumed = state
            .update((101_024, 202_048), Duration::from_secs(1))
            .unwrap();
        assert_eq!(resumed, (1024, 2048));
    }
}
