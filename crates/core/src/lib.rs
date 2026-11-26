pub mod clock;
pub mod config;
pub mod format;
pub mod smoothing;
pub mod temp;

use serde::{Deserialize, Serialize};

/// 网卡枚举结果；ifindex 只用于当次事件关联，跨重启唯一键是 `stable`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterfaceInfo {
    pub name: String,
    pub ifindex: u32,
    /// `mac:<小写MAC>`；无永久 MAC 时为 `name:<接口名>`。
    pub stable: String,
    pub mac: Option<String>,
    pub operstate: String,
    pub carrier: bool,
    pub is_loopback: bool,
}

/// 某网卡在一个统计区间内的原始字节增量。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterfaceTraffic {
    pub stable: String,
    pub name: String,
    pub rx: u64,
    pub tx: u64,
}

/// `scope=day|month` 的流量汇总；`interfaces` 为各网卡分解。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrafficSummary {
    pub scope: String,
    pub start: String,
    pub end: String,
    pub rx: u64,
    pub tx: u64,
    pub interfaces: Vec<InterfaceTraffic>,
}

/// 告警状态机事件；`active=false` 表示恢复。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertEvent {
    pub level: String,
    pub code: String,
    pub message: String,
    pub active: bool,
    /// 事件产生的墙上时间（Unix 秒），用于冷却与去重展示。
    pub ts_secs: i64,
}

/// 当前桌面配色，仅用于渲染资源选择。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorScheme {
    #[default]
    Dark,
    Light,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetSample {
    pub iface: String,
    pub rx_bps: u64,
    pub tx_bps: u64,
    pub rx_total: u64,
    pub tx_total: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CpuSample {
    pub total_pct: f32,
    pub per_core: Vec<f32>,
    pub freq_mhz: Option<u32>,
    pub load: Option<[f32; 3]>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemSample {
    pub used_pct: f32,
    pub total: u64,
    pub available: u64,
    pub swap_pct: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskSample {
    pub read_bps: u64,
    pub write_bps: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuSample {
    pub name: String,
    pub util_pct: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    /// Intel 等后端可报告频率；未知时省略。
    #[serde(default)]
    pub freq_mhz: Option<u32>,
    /// 中间帧沿用上一读数时为 `true`，避免把旧值当成新采样。
    #[serde(default)]
    pub stale: bool,
    /// 读数实际观测时间（`CLOCK_MONOTONIC` 毫秒）。
    #[serde(default)]
    pub observed_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TempSample {
    pub label: String,
    pub celsius: f32,
}

/// 一个挂载点的容量；采集失败时整个条目省略。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilesystemSample {
    pub mount: String,
    pub total: u64,
    pub available: u64,
    pub used_pct: f32,
}

/// 多电池按满容量加权后的整体状态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatterySample {
    pub percent: f32,
    pub status: String,
}

/// 进程级流量；仅由可选特权 helper 聚合后提供。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessTrafficSample {
    pub pid: u32,
    pub name: String,
    pub rx_bps: u64,
    pub tx_bps: u64,
    pub rx_total: u64,
    pub tx_total: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub ts_ms: u64,
    pub net: NetSample,
    pub cpu: CpuSample,
    pub mem: MemSample,
    /// 可选键：M1 未采集时为 `None`（JSON 输出 `null`）。
    pub disk: Option<DiskSample>,
    /// 主 GPU 兼容别名；多 GPU 时与 `gpus[0]` 相同。
    pub gpu: Option<GpuSample>,
    /// 全部可用 GPU；未检测到后端时为空。
    #[serde(default)]
    pub gpus: Vec<GpuSample>,
    pub temps: Vec<TempSample>,
    /// 挂载点容量；未采集时为 `None`。
    #[serde(default)]
    pub filesystems: Option<Vec<FilesystemSample>>,
    #[serde(default)]
    pub battery: Option<BatterySample>,
    #[serde(default)]
    pub uptime_s: Option<u64>,
    /// 可选：进程级流量；`backend=off` 或 helper 不可用时省略。
    #[serde(default)]
    pub processes: Option<Vec<ProcessTrafficSample>>,
    pub plugin: std::collections::BTreeMap<String, String>,
}

impl Snapshot {
    /// 显式构造器：扩展字段保持“未采集”而不是伪造零值。
    pub fn new(ts_ms: u64, net: NetSample, cpu: CpuSample, mem: MemSample) -> Self {
        Self {
            ts_ms,
            net,
            cpu,
            mem,
            disk: None,
            gpu: None,
            gpus: Vec::new(),
            temps: Vec::new(),
            filesystems: None,
            battery: None,
            uptime_s: None,
            processes: None,
            plugin: std::collections::BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructor_keeps_optional_metrics_unset() {
        let snapshot = Snapshot::new(
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
        );
        assert!(snapshot.disk.is_none());
        assert!(snapshot.gpu.is_none());
        assert!(snapshot.gpus.is_empty());
        assert!(snapshot.temps.is_empty());
        assert!(snapshot.filesystems.is_none());
        assert!(snapshot.battery.is_none());
        assert!(snapshot.uptime_s.is_none());
        assert!(snapshot.processes.is_none());
        assert!(snapshot.plugin.is_empty());
    }
}
