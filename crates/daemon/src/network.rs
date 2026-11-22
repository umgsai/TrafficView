//! Linux 网卡发现、默认路由、白/黑名单与自动选择。
//!
//! 选择顺序固定：白名单 → 非黑名单默认路由 → 活跃接口中速率最大者 →
//! `NoActiveInterface`。所有测试使用 fixture，不依赖本机网卡。

use std::{collections::HashMap, fmt, fs, io, path::Path};

use globset::{Glob, GlobSet, GlobSetBuilder};
use trafficview_core::InterfaceInfo;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectError {
    /// 没有任何可用接口；daemon 与历史查询必须继续存活。
    NoActiveInterface,
}

impl fmt::Display for SelectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoActiveInterface => write!(f, "no active network interface"),
        }
    }
}

impl std::error::Error for SelectError {}

/// 默认黑名单；与 `Config::default()` 保持一致。
pub const DEFAULT_BLACKLIST: [&str; 6] = ["lo", "docker*", "veth*", "br-*", "virbr*", "tun*"];

/// 从 `/sys/class/net` 发现网卡；ifindex 仅用于当次事件关联。
pub fn discover(sys_root: &Path) -> Vec<InterfaceInfo> {
    let mut interfaces: Vec<InterfaceInfo> = fs::read_dir(sys_root)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| {
                    let name = entry.file_name().into_string().ok()?;
                    let dir = entry.path();
                    let ifindex = read_trimmed(&dir.join("ifindex"))
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(0);
                    let mac = read_trimmed(&dir.join("address"))
                        .filter(|value| value != "00:00:00:00:00:00")
                        .map(|value| value.to_ascii_lowercase());
                    let operstate =
                        read_trimmed(&dir.join("operstate")).unwrap_or_else(|| "unknown".into());
                    let carrier = read_trimmed(&dir.join("carrier"))
                        .map(|value| value == "1")
                        .unwrap_or(false);
                    let kind = read_trimmed(&dir.join("type"))
                        .and_then(|value| value.parse::<u32>().ok())
                        .unwrap_or(0);
                    Some(InterfaceInfo {
                        stable: stable_key(&name, mac.as_deref()),
                        name,
                        ifindex,
                        mac,
                        operstate,
                        carrier,
                        is_loopback: kind == 772,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    interfaces.sort_by(|a, b| a.name.cmp(&b.name));
    interfaces
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// 稳定键：优先 MAC；无永久 MAC 时退回接口名。
pub fn stable_key(name: &str, mac: Option<&str>) -> String {
    match mac {
        Some(mac) if !mac.is_empty() && mac != "00:00:00:00:00:00" => {
            format!("mac:{}", mac.to_ascii_lowercase())
        }
        _ => format!("name:{name}"),
    }
}

/// 从 `/proc/net/route` 文本解析默认路由网卡。
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

/// 按固定优先级选择监控网卡。
#[derive(Debug, Clone)]
pub struct NetSelector {
    whitelist: GlobSet,
    blacklist: GlobSet,
    default_route: Option<String>,
}

impl NetSelector {
    pub fn new<S: AsRef<str>>(
        whitelist: &[S],
        blacklist: &[S],
        default_route: Option<String>,
    ) -> Result<Self, globset::Error> {
        Ok(Self {
            whitelist: build(whitelist)?,
            blacklist: build(blacklist)?,
            default_route,
        })
    }

    pub fn default_route(&self) -> Option<&str> {
        self.default_route.as_deref()
    }

    pub fn choose<'a>(
        &self,
        interfaces: &'a [InterfaceInfo],
        rates: &HashMap<String, u64>,
    ) -> Result<&'a InterfaceInfo, SelectError> {
        let usable = |iface: &InterfaceInfo| is_up(iface) && !iface.is_loopback;

        // 1. 白名单优先，即使同时命中黑名单。
        if let Some(found) = interfaces
            .iter()
            .filter(|iface| usable(iface) && self.whitelist.is_match(&iface.name))
            .min_by(|a, b| a.name.cmp(&b.name))
        {
            return Ok(found);
        }
        // 2. 非黑名单的默认路由接口。
        if let Some(name) = self.default_route.as_deref() {
            if let Some(found) = interfaces
                .iter()
                .find(|iface| iface.name == name && usable(iface) && !self.blacklisted(iface))
            {
                return Ok(found);
            }
        }
        // 3. 活跃接口中当前总速率最大者。
        interfaces
            .iter()
            .filter(|iface| usable(iface) && !self.blacklisted(iface))
            .max_by_key(|iface| rates.get(&iface.name).copied().unwrap_or(0))
            .ok_or(SelectError::NoActiveInterface)
    }

    fn blacklisted(&self, iface: &InterfaceInfo) -> bool {
        self.blacklist.is_match(&iface.name)
    }
}

fn is_up(iface: &InterfaceInfo) -> bool {
    matches!(iface.operstate.as_str(), "up" | "unknown")
}

fn build<S: AsRef<str>>(patterns: &[S]) -> Result<GlobSet, globset::Error> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern.as_ref())?);
    }
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_route() {
        const ROUTE: &str = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\nenp0s3\t00000000\t020010AC\t0003\t0\t0\t100\t00000000\t0\t0\t0\n";
        assert_eq!(parse_default_route(ROUTE).as_deref(), Some("enp0s3"));
        assert_eq!(parse_default_route(""), None);
    }

    #[test]
    fn stable_key_prefers_mac_but_falls_back_to_name() {
        assert_eq!(
            stable_key("eth0", Some("AA:BB:CC:DD:EE:FF")),
            "mac:aa:bb:cc:dd:ee:ff"
        );
        assert_eq!(stable_key("tun0", None), "name:tun0");
        assert_eq!(stable_key("eth0", Some("00:00:00:00:00:00")), "name:eth0");
    }

    #[test]
    fn candidate_sysfs_layout_is_discovered() {
        let dir = tempfile::tempdir().unwrap();
        let eth0 = dir.path().join("eth0");
        fs::create_dir(&eth0).unwrap();
        fs::write(eth0.join("ifindex"), "2\n").unwrap();
        fs::write(eth0.join("address"), "AA:BB:CC:DD:EE:FF\n").unwrap();
        fs::write(eth0.join("operstate"), "up\n").unwrap();
        fs::write(eth0.join("carrier"), "1\n").unwrap();
        fs::write(eth0.join("type"), "1\n").unwrap();
        let lo = dir.path().join("lo");
        fs::create_dir(&lo).unwrap();
        fs::write(lo.join("ifindex"), "1\n").unwrap();
        fs::write(lo.join("address"), "00:00:00:00:00:00\n").unwrap();
        fs::write(lo.join("operstate"), "unknown\n").unwrap();
        fs::write(lo.join("type"), "772\n").unwrap();

        let interfaces = discover(dir.path());
        assert_eq!(interfaces.len(), 2);
        assert_eq!(interfaces[0].name, "eth0");
        assert_eq!(interfaces[0].stable, "mac:aa:bb:cc:dd:ee:ff");
        assert_eq!(interfaces[0].ifindex, 2);
        assert!(interfaces[1].is_loopback);
        assert_eq!(interfaces[1].stable, "name:lo");
    }
}
