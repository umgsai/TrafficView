pub mod clock;
pub mod config;
pub mod format;

use serde::{Deserialize, Serialize};

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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TempSample {
    pub label: String,
    pub celsius: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub ts_ms: u64,
    pub net: NetSample,
    pub cpu: CpuSample,
    pub mem: MemSample,
    /// 可选键：M1 未采集时为 `None`（JSON 输出 `null`）。
    pub disk: Option<DiskSample>,
    pub gpu: Option<GpuSample>,
    pub temps: Vec<TempSample>,
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
            temps: Vec::new(),
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
        assert!(snapshot.temps.is_empty());
        assert!(snapshot.plugin.is_empty());
    }
}
