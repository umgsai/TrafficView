//! AF_PACKET 进程流量归属：纯解析、映射与聚合。
//!
//! 该模块不打开任何特权资源，可完全用 fixture 测试；真正的 `CAP_NET_RAW`
//! 抓包与 system D-Bus 服务位于 `bin/trafficviewd-ebpf.rs`。

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use trafficview_core::ProcessTrafficSample;

/// socket 表与 inode→PID 的默认刷新周期。
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Protocol {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Rx,
    Tx,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketEntry {
    pub protocol: Protocol,
    pub local_port: u16,
    pub remote_port: u16,
    pub inode: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketFlow {
    pub protocol: Protocol,
    pub src_port: u16,
    pub dst_port: u16,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessMeta {
    pub pid: u32,
    pub name: String,
    pub uid: u32,
    pub start_time: u64,
}

/// 带 UID 的内部样本；对外返回前会按调用者 UID 过滤。
#[derive(Debug, Clone, PartialEq)]
pub struct TaggedSample {
    pub uid: u32,
    pub sample: ProcessTrafficSample,
}

/// 解析 `/proc/net/{tcp,udp,tcp6,udp6}`；跳过表头与 inode=0 的项。
pub fn parse_proc_net(text: &str, protocol: Protocol) -> Vec<SocketEntry> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 || !fields[0].ends_with(':') {
                return None;
            }
            Some(SocketEntry {
                protocol,
                local_port: parse_hex_port(fields[1])?,
                remote_port: parse_hex_port(fields[2])?,
                inode: fields[9].parse().ok()?,
            })
        })
        .filter(|entry| entry.inode != 0)
        .collect()
}

fn parse_hex_port(field: &str) -> Option<u16> {
    let (_, port) = field.rsplit_once(':')?;
    u16::from_str_radix(port, 16).ok()
}

/// 从包帧中提取 TCP/UDP 端口；支持 Ethernet、单层 VLAN、IPv4 与 IPv6。
pub fn parse_packet(frame: &[u8]) -> Option<PacketFlow> {
    if frame.len() < 14 {
        return None;
    }
    let bytes = frame.len() as u64;
    let mut offset = 12;
    let mut ethertype = u16::from_be_bytes([frame[offset], frame[offset + 1]]);
    offset += 2;
    while ethertype == 0x8100 || ethertype == 0x88a8 {
        let header = frame.get(offset..offset + 4)?;
        ethertype = u16::from_be_bytes([header[2], header[3]]);
        offset += 4;
    }
    match ethertype {
        0x0800 => parse_ipv4(frame.get(offset..)?, bytes),
        0x86dd => parse_ipv6(frame.get(offset..)?, bytes),
        _ => None,
    }
}

fn parse_ipv4(header: &[u8], bytes: u64) -> Option<PacketFlow> {
    if header.len() < 20 || header[0] >> 4 != 4 {
        return None;
    }
    let ihl = usize::from(header[0] & 0x0f) * 4;
    if ihl < 20 || header.len() < ihl {
        return None;
    }
    // 非首片没有完整端口；丢弃以免把流量记到错误进程。
    let fragment = u16::from_be_bytes([header[6], header[7]]);
    if fragment & 0x1fff != 0 {
        return None;
    }
    let protocol = match header[9] {
        6 => Protocol::Tcp,
        17 => Protocol::Udp,
        _ => return None,
    };
    let ports = header.get(ihl..ihl + 4)?;
    Some(PacketFlow {
        protocol,
        src_port: u16::from_be_bytes([ports[0], ports[1]]),
        dst_port: u16::from_be_bytes([ports[2], ports[3]]),
        bytes,
    })
}

fn parse_ipv6(header: &[u8], bytes: u64) -> Option<PacketFlow> {
    if header.len() < 44 || header[0] >> 4 != 6 {
        return None;
    }
    // 仅处理无扩展头的 TCP/UDP；其余保守地归入 unattributed。
    let protocol = match header[6] {
        6 => Protocol::Tcp,
        17 => Protocol::Udp,
        _ => return None,
    };
    Some(PacketFlow {
        protocol,
        src_port: u16::from_be_bytes([header[40], header[41]]),
        dst_port: u16::from_be_bytes([header[42], header[43]]),
        bytes,
    })
}

/// 本地端口到 inode 的索引；收到/发出按端口方向判定。
#[derive(Debug, Clone, Default)]
pub struct PortIndex {
    by_port: HashMap<(Protocol, u16), u64>,
}

impl PortIndex {
    pub fn build(sockets: &[SocketEntry]) -> Self {
        let mut by_port = HashMap::new();
        for socket in sockets {
            if socket.local_port != 0 {
                by_port.insert((socket.protocol, socket.local_port), socket.inode);
            }
        }
        Self { by_port }
    }

    pub fn direction(&self, flow: &PacketFlow) -> Option<(Direction, u64)> {
        if let Some(&inode) = self.by_port.get(&(flow.protocol, flow.dst_port)) {
            return Some((Direction::Rx, inode));
        }
        self.by_port
            .get(&(flow.protocol, flow.src_port))
            .map(|&inode| (Direction::Tx, inode))
    }
}

pub fn parse_socket_link(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// 扫描 `/proc/<pid>/fd` 建立 inode→PID 映射（短周期刷新）。
pub fn scan_inode_pids(proc_root: &Path) -> HashMap<u64, u32> {
    let Ok(entries) = fs::read_dir(proc_root) else {
        return HashMap::new();
    };
    let mut map = HashMap::new();
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(fds) = fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(link) = fs::read_link(fd.path()) else {
                continue;
            };
            if let Some(inode) = parse_socket_link(&link.to_string_lossy()) {
                map.insert(inode, pid);
            }
        }
    }
    map
}

/// 读取进程 UID、comm 与 starttime；starttime 用于识别 PID 重用。
pub fn read_process_meta(proc_root: &Path, pid: u32) -> Option<ProcessMeta> {
    let dir = proc_root.join(pid.to_string());
    let status = fs::read_to_string(dir.join("status")).ok()?;
    let uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let stat = fs::read_to_string(dir.join("stat")).ok()?;
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let name = stat[open + 1..close].to_owned();
    let start_time = stat[close + 1..].split_whitespace().nth(19)?.parse().ok()?;
    Some(ProcessMeta {
        pid,
        name,
        uid,
        start_time,
    })
}

/// 内核/helper 计数器可能 reset；倒退时返回 `None`，只重建基线。
pub fn counter_delta(previous: u64, current: u64) -> Option<u64> {
    current.checked_sub(previous)
}

#[derive(Debug, Clone)]
struct Tracked {
    start_time: u64,
    name: String,
    uid: u32,
    rx_total: u64,
    tx_total: u64,
}

/// 把每周期字节增量转换为 bps 与累计值；进程退出即删除，PID 重用则清零。
pub struct RateTracker {
    last: Instant,
    entries: HashMap<u32, Tracked>,
}

impl RateTracker {
    pub fn new(now: Instant) -> Self {
        Self {
            last: now,
            entries: HashMap::new(),
        }
    }

    pub fn tick(&mut self, now: Instant, deltas: &[(ProcessMeta, u64, u64)]) -> Vec<TaggedSample> {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        let elapsed = elapsed.max(1e-9);
        let mut seen = HashSet::new();
        let mut samples = Vec::with_capacity(deltas.len());
        for (meta, rx_delta, tx_delta) in deltas {
            seen.insert(meta.pid);
            let tracked = self.entries.entry(meta.pid).or_insert_with(|| Tracked {
                start_time: meta.start_time,
                name: meta.name.clone(),
                uid: meta.uid,
                rx_total: 0,
                tx_total: 0,
            });
            if tracked.start_time != meta.start_time {
                *tracked = Tracked {
                    start_time: meta.start_time,
                    name: meta.name.clone(),
                    uid: meta.uid,
                    rx_total: 0,
                    tx_total: 0,
                };
            }
            tracked.name.clone_from(&meta.name);
            tracked.uid = meta.uid;
            tracked.rx_total = tracked.rx_total.saturating_add(*rx_delta);
            tracked.tx_total = tracked.tx_total.saturating_add(*tx_delta);
            samples.push(TaggedSample {
                uid: meta.uid,
                sample: ProcessTrafficSample {
                    pid: meta.pid,
                    name: meta.name.clone(),
                    rx_bps: to_bps(*rx_delta, elapsed),
                    tx_bps: to_bps(*tx_delta, elapsed),
                    rx_total: tracked.rx_total,
                    tx_total: tracked.tx_total,
                },
            });
        }
        self.entries.retain(|pid, _| seen.contains(pid));
        self.last = now;
        samples.sort_by(|left, right| {
            let left_total = left.sample.rx_bps + left.sample.tx_bps;
            let right_total = right.sample.rx_bps + right.sample.tx_bps;
            right_total
                .cmp(&left_total)
                .then_with(|| left.sample.pid.cmp(&right.sample.pid))
        });
        samples
    }
}

fn to_bps(bytes: u64, seconds: f64) -> u64 {
    (bytes as f64 / seconds) as u64
}

/// 只保留调用者 UID 的进程；root（uid 0）可取全部。
pub fn filter_by_uid(samples: Vec<TaggedSample>, caller_uid: u32) -> Vec<ProcessTrafficSample> {
    samples
        .into_iter()
        .filter(|tagged| caller_uid == 0 || tagged.uid == caller_uid)
        .map(|tagged| tagged.sample)
        .collect()
}

/// AF_PACKET 聚合器：帧归属、周期差分与总量维护。
pub struct Aggregator {
    proc_root: PathBuf,
    net_root: PathBuf,
    tracker: RateTracker,
    port_index: PortIndex,
    inode_pid: HashMap<u64, u32>,
    pending: HashMap<u32, (u64, u64)>,
    last_tables: Instant,
    refresh_interval: Duration,
    unattributed: u64,
}

impl Aggregator {
    pub fn new(proc_root: PathBuf, now: Instant) -> Self {
        let net_root = proc_root.join("net");
        Self::with_refresh(proc_root, net_root, now, REFRESH_INTERVAL)
    }

    pub fn with_refresh(
        proc_root: PathBuf,
        net_root: PathBuf,
        now: Instant,
        refresh_interval: Duration,
    ) -> Self {
        let mut aggregator = Self {
            proc_root,
            net_root,
            tracker: RateTracker::new(now),
            port_index: PortIndex::default(),
            inode_pid: HashMap::new(),
            pending: HashMap::new(),
            last_tables: now,
            refresh_interval,
            unattributed: 0,
        };
        aggregator.reload_tables();
        aggregator
    }

    fn reload_tables(&mut self) {
        let mut sockets = Vec::new();
        for (name, protocol) in [
            ("tcp", Protocol::Tcp),
            ("tcp6", Protocol::Tcp),
            ("udp", Protocol::Udp),
            ("udp6", Protocol::Udp),
        ] {
            if let Ok(text) = fs::read_to_string(self.net_root.join(name)) {
                sockets.extend(parse_proc_net(&text, protocol));
            }
        }
        self.port_index = PortIndex::build(&sockets);
        self.inode_pid = scan_inode_pids(&self.proc_root);
    }

    /// 喂入一帧；无法归属的字节单独累计，绝不分配给随机 PID。
    pub fn ingest(&mut self, frame: &[u8], now: Instant) {
        if now.saturating_duration_since(self.last_tables) >= self.refresh_interval {
            self.reload_tables();
            self.last_tables = now;
        }
        let Some(flow) = parse_packet(frame) else {
            self.unattributed += frame.len() as u64;
            return;
        };
        let Some((direction, inode)) = self.port_index.direction(&flow) else {
            self.unattributed += flow.bytes;
            return;
        };
        let Some(&pid) = self.inode_pid.get(&inode) else {
            self.unattributed += flow.bytes;
            return;
        };
        let entry = self.pending.entry(pid).or_insert((0, 0));
        match direction {
            Direction::Rx => entry.0 = entry.0.saturating_add(flow.bytes),
            Direction::Tx => entry.1 = entry.1.saturating_add(flow.bytes),
        }
    }

    /// 产出本周期样本；仅包含当前仍存活且仍持有 socket 的进程。
    pub fn refresh(&mut self, now: Instant) -> Vec<TaggedSample> {
        if now.saturating_duration_since(self.last_tables) >= self.refresh_interval {
            self.reload_tables();
            self.last_tables = now;
        }
        let mut pids: HashSet<u32> = self.pending.keys().copied().collect();
        pids.extend(self.inode_pid.values().copied());
        let mut deltas = Vec::with_capacity(pids.len());
        for pid in pids {
            let Some(meta) = read_process_meta(&self.proc_root, pid) else {
                continue;
            };
            let (rx, tx) = self.pending.get(&pid).copied().unwrap_or((0, 0));
            deltas.push((meta, rx, tx));
        }
        self.pending.clear();
        self.tracker.tick(now, &deltas)
    }

    pub fn unattributed_bytes(&self) -> u64 {
        self.unattributed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    const TCP_FIXTURE: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4242 1 0000000000000000 100 0 0 10 0\n";

    fn ethernet_tcp(src_port: u16, dst_port: u16, payload: usize) -> Vec<u8> {
        let mut frame = vec![0u8; 14 + 20 + 20 + payload];
        frame[12..14].copy_from_slice(&[0x08, 0x00]);
        frame[14] = 0x45;
        frame[23] = 6;
        frame[34..36].copy_from_slice(&src_port.to_be_bytes());
        frame[36..38].copy_from_slice(&dst_port.to_be_bytes());
        frame
    }

    fn fixture_process(root: &Path, pid: u32, name: &str, uid: u32, inode: u64) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(dir.join("fd")).unwrap();
        fs::write(
            dir.join("status"),
            format!("Name:\t{name}\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
        )
        .unwrap();
        let stat = format!("{pid} ({name}) S{} 424242 22 1", " 1".repeat(18));
        fs::write(dir.join("stat"), stat).unwrap();
        symlink(format!("socket:[{inode}]"), dir.join("fd").join("3")).unwrap();
    }

    fn fixture_net(root: &Path) {
        fs::create_dir_all(root.join("net")).unwrap();
        fs::write(root.join("net/tcp"), TCP_FIXTURE).unwrap();
    }

    #[test]
    fn proc_net_and_socket_links_are_parsed() {
        let sockets = parse_proc_net(TCP_FIXTURE, Protocol::Tcp);
        assert_eq!(
            sockets,
            vec![SocketEntry {
                protocol: Protocol::Tcp,
                local_port: 8080,
                remote_port: 0,
                inode: 4242,
            }]
        );
        assert_eq!(parse_socket_link("socket:[4242]"), Some(4242));
        assert_eq!(parse_socket_link("pipe:[1]"), None);
    }

    #[test]
    fn packet_parser_handles_vlan_ipv4_and_ipv6() {
        let frame = ethernet_tcp(50000, 8080, 10);
        let flow = parse_packet(&frame).unwrap();
        assert_eq!(flow.protocol, Protocol::Tcp);
        assert_eq!(flow.src_port, 50000);
        assert_eq!(flow.dst_port, 8080);
        assert_eq!(flow.bytes, frame.len() as u64);

        let mut vlan = vec![0u8; 18];
        vlan[12..14].copy_from_slice(&[0x81, 0x00]);
        vlan[16..18].copy_from_slice(&[0x08, 0x00]);
        vlan.extend_from_slice(&frame[14..]);
        assert_eq!(parse_packet(&vlan).unwrap().dst_port, 8080);

        let mut ipv6 = vec![0u8; 14 + 40 + 4];
        ipv6[12..14].copy_from_slice(&[0x86, 0xdd]);
        ipv6[14] = 0x60;
        ipv6[20] = 17;
        ipv6[54..56].copy_from_slice(&5353u16.to_be_bytes());
        ipv6[56..58].copy_from_slice(&1000u16.to_be_bytes());
        let flow = parse_packet(&ipv6).unwrap();
        assert_eq!(flow.protocol, Protocol::Udp);
        assert_eq!(flow.dst_port, 1000);

        assert!(parse_packet(&[0u8; 10]).is_none());
    }

    #[test]
    fn port_index_maps_rx_and_tx_to_the_same_inode() {
        let index = PortIndex::build(&[SocketEntry {
            protocol: Protocol::Tcp,
            local_port: 8080,
            remote_port: 0,
            inode: 4242,
        }]);
        let rx = PacketFlow {
            protocol: Protocol::Tcp,
            src_port: 50000,
            dst_port: 8080,
            bytes: 10,
        };
        let tx = PacketFlow {
            protocol: Protocol::Tcp,
            src_port: 8080,
            dst_port: 50000,
            bytes: 10,
        };
        assert_eq!(index.direction(&rx), Some((Direction::Rx, 4242)));
        assert_eq!(index.direction(&tx), Some((Direction::Tx, 4242)));
    }

    #[test]
    fn process_meta_reads_uid_name_and_start_time() {
        let dir = tempfile::tempdir().unwrap();
        fixture_process(dir.path(), 123, "demo", 1000, 4242);
        let meta = read_process_meta(dir.path(), 123).unwrap();
        assert_eq!(meta.name, "demo");
        assert_eq!(meta.uid, 1000);
        assert_eq!(meta.start_time, 424242);
        assert!(read_process_meta(dir.path(), 999).is_none());
    }

    #[test]
    fn rate_tracker_reports_deltas_drops_exited_and_resets_reused_pids() {
        let base = Instant::now();
        let mut tracker = RateTracker::new(base);
        let meta = ProcessMeta {
            pid: 7,
            name: "demo".into(),
            uid: 1000,
            start_time: 100,
        };
        let first = tracker.tick(base + Duration::from_secs(2), &[(meta.clone(), 2048, 1024)]);
        assert_eq!(first[0].sample.rx_bps, 1024);
        assert_eq!(first[0].sample.tx_bps, 512);
        assert_eq!(first[0].sample.rx_total, 2048);

        let reused = ProcessMeta {
            start_time: 200,
            ..meta.clone()
        };
        let second = tracker.tick(base + Duration::from_secs(4), &[(reused, 100, 0)]);
        assert_eq!(
            second[0].sample.rx_total, 100,
            "PID reuse must reset totals"
        );

        let third = tracker.tick(base + Duration::from_secs(6), &[]);
        assert!(third.is_empty(), "exited processes must be removed");
    }

    #[test]
    fn counter_reset_is_not_reported_as_traffic() {
        assert_eq!(counter_delta(100, 160), Some(60));
        assert_eq!(counter_delta(160, 10), None);
    }

    #[test]
    fn uid_filter_hides_other_users_and_root_sees_all() {
        let samples = vec![
            TaggedSample {
                uid: 1000,
                sample: ProcessTrafficSample {
                    pid: 1,
                    name: "mine".into(),
                    rx_bps: 1,
                    tx_bps: 1,
                    rx_total: 1,
                    tx_total: 1,
                },
            },
            TaggedSample {
                uid: 1001,
                sample: ProcessTrafficSample {
                    pid: 2,
                    name: "other".into(),
                    rx_bps: 1,
                    tx_bps: 1,
                    rx_total: 1,
                    tx_total: 1,
                },
            },
        ];
        let mine = filter_by_uid(samples.clone(), 1000);
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].pid, 1);
        assert_eq!(filter_by_uid(samples, 0).len(), 2);
    }

    #[test]
    fn aggregator_attributes_frames_to_the_socket_owner() {
        let dir = tempfile::tempdir().unwrap();
        fixture_net(dir.path());
        fixture_process(dir.path(), 123, "demo", 1000, 4242);
        let base = Instant::now();
        let mut aggregator = Aggregator::with_refresh(
            dir.path().into(),
            dir.path().join("net"),
            base,
            REFRESH_INTERVAL,
        );

        let rx = ethernet_tcp(50000, 8080, 10);
        aggregator.ingest(&rx, base);
        let samples = aggregator.refresh(base + Duration::from_secs(2));
        let sample = samples
            .iter()
            .find(|tagged| tagged.sample.pid == 123)
            .expect("socket owner should be attributed");
        assert_eq!(sample.sample.rx_bps, rx.len() as u64 / 2);
        assert_eq!(sample.sample.tx_bps, 0);
        assert_eq!(aggregator.unattributed_bytes(), 0);

        let unknown = ethernet_tcp(50000, 9999, 10);
        aggregator.ingest(&unknown, base + Duration::from_secs(2));
        assert_eq!(
            aggregator.unattributed_bytes(),
            unknown.len() as u64,
            "unmatched ports must not be assigned to a random PID"
        );
    }
}
