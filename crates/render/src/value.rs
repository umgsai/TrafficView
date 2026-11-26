//! `DisplayItem` 取值、单位格式与模板替换。
//!
//! 该模块是皮肤与内置布局共用的唯一取值注册表；未采集的可选值一般显式显示 `--`，
//! 不伪造零值。`cpu_temp` 在无读数时由 [`is_available`] 判定为不可用并整行隐藏，
//! 因此不会绘制占位符。

use trafficview_core::{
    config::RateUnit,
    format::{format_bit_rate, format_rate},
    Snapshot,
};

pub const BUILTIN_IDS: [&str; 5] = ["net_up", "net_down", "cpu", "mem", "cpu_temp"];

pub fn is_known_id(id: &str) -> bool {
    BUILTIN_IDS.contains(&id)
        || id
            .strip_prefix("plugin.")
            .is_some_and(|rest| !rest.trim().is_empty())
}

/// 显示项当前是否有可绘制数据；`cpu_temp` 无读数时整行隐藏。
pub fn is_available(id: &str, snapshot: &Snapshot) -> bool {
    id != "cpu_temp" || snapshot.cpu_temp.is_some()
}

/// 阈值比较使用的数值。速率始终以字节/秒参与阈值判断。
pub fn numeric_value(id: &str, snapshot: &Snapshot) -> Option<f64> {
    match id {
        "net_up" => Some(snapshot.net.tx_bps as f64),
        "net_down" => Some(snapshot.net.rx_bps as f64),
        "cpu" => Some(f64::from(snapshot.cpu.total_pct)),
        "mem" => Some(f64::from(snapshot.mem.used_pct)),
        "cpu_temp" => snapshot.cpu_temp.map(f64::from),
        other => other
            .strip_prefix("plugin.")
            .and_then(|key| snapshot.plugin.get(key))
            .and_then(|text| text.parse::<f64>().ok()),
    }
}

/// 格式化后的显示文本。模板中只有 `{value}` 会被替换。
///
/// 调用方应先通过 [`is_available`] 判断该项当前是否有可绘制数据；例如
/// `cpu_temp` 无读数时应整行隐藏，而不是绘制这里返回的 `--`。
pub fn display_value(id: &str, snapshot: &Snapshot, units: RateUnit, format: &str) -> String {
    if !is_known_id(id) {
        return String::new();
    }
    let Some(value) = numeric_value(id, snapshot) else {
        return "--".into();
    };
    let text = match id {
        "net_up" => format_rate_with_units(snapshot.net.tx_bps, units),
        "net_down" => format_rate_with_units(snapshot.net.rx_bps, units),
        "cpu" | "mem" => format!("{value:.1}%"),
        "cpu_temp" => format!("{value:.0}°C"),
        _ => format!("{value:.1}"),
    };
    let template = if format.trim().is_empty() {
        "{value}"
    } else {
        format
    };
    template.replace("{value}", &text)
}

fn format_rate_with_units(bytes_per_sec: u64, units: RateUnit) -> String {
    match units {
        RateUnit::Bytes => format_rate(bytes_per_sec),
        RateUnit::Bits => format_bit_rate(bytes_per_sec),
    }
}
