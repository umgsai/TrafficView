pub mod clock;
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
pub struct Snapshot {
    pub ts_ms: u64,
    pub net: NetSample,
    pub cpu: CpuSample,
    pub mem: MemSample,
}
