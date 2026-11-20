use std::{
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
}
