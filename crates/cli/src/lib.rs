//! 面板/脚本可消费的状态格式出口。所有格式都只输出一行，不写日志到 stdout。

use serde_json::{json, Error as JsonError};
use trafficview_core::{
    format::{format_bit_rate, format_rate},
    Snapshot,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum StatusFormat {
    Text,
    Json,
    Waybar,
    Polybar,
    Tmux,
}

fn rate(bytes_per_sec: u64, bits: bool) -> String {
    if bits {
        format_bit_rate(bytes_per_sec)
    } else {
        format_rate(bytes_per_sec)
    }
}

/// waybar `return-type: json` 的单行输出。
pub fn format_waybar(snapshot: &Snapshot, bits: bool) -> Result<String, JsonError> {
    let text = format!(
        "↓{} ↑{} CPU {:.0}% MEM {:.0}%",
        rate(snapshot.net.rx_bps, bits),
        rate(snapshot.net.tx_bps, bits),
        snapshot.cpu.total_pct,
        snapshot.mem.used_pct
    );
    let tooltip = format!(
        "TrafficView\niface: {}\nDownload: {}\nUpload: {}\nCPU: {:.1}%\nMEM: {:.1}%\nSwap: {:.1}%\nts: {} ms",
        snapshot.net.iface,
        rate(snapshot.net.rx_bps, bits),
        rate(snapshot.net.tx_bps, bits),
        snapshot.cpu.total_pct,
        snapshot.mem.used_pct,
        snapshot.mem.swap_pct,
        snapshot.ts_ms
    );
    serde_json::to_string(&json!({
        "text": text,
        "tooltip": tooltip,
        "class": "normal",
        "alt": "normal",
    }))
}

/// daemon 不可达时的显式状态；仅 `--allow-disconnected` 才输出。
pub fn format_disconnected() -> Result<String, JsonError> {
    serde_json::to_string(&json!({
        "text": "TrafficView: --",
        "tooltip": "trafficviewd is not reachable",
        "class": "disconnected",
        "alt": "disconnected",
    }))
}

pub fn format_paused() -> Result<String, JsonError> {
    serde_json::to_string(&json!({
        "text": "TrafficView: paused",
        "tooltip": "trafficviewd is paused",
        "class": "paused",
        "alt": "paused",
    }))
}

pub fn format_polybar(snapshot: &Snapshot, bits: bool) -> String {
    format!(
        "↓{} ↑{} CPU {:.0}% MEM {:.0}%",
        rate(snapshot.net.rx_bps, bits),
        rate(snapshot.net.tx_bps, bits),
        snapshot.cpu.total_pct,
        snapshot.mem.used_pct
    )
}

pub fn format_tmux(snapshot: &Snapshot, bits: bool) -> String {
    format!(
        "↓{} ↑{} CPU {:.0}% MEM {:.0}%",
        rate(snapshot.net.rx_bps, bits),
        rate(snapshot.net.tx_bps, bits),
        snapshot.cpu.total_pct,
        snapshot.mem.used_pct
    )
}

pub fn format_text(snapshot: &Snapshot, bits: bool) -> String {
    format!(
        "iface={} ↓{} ↑{} CPU {:.0}% MEM {:.0}% ts={}ms",
        snapshot.net.iface,
        rate(snapshot.net.rx_bps, bits),
        rate(snapshot.net.tx_bps, bits),
        snapshot.cpu.total_pct,
        snapshot.mem.used_pct,
        snapshot.ts_ms
    )
}
