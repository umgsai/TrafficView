//! `DisplayItem` 取值、单位格式与模板替换。
//!
//! 该模块是皮肤与内置布局共用的唯一取值注册表；未采集的可选值一般显式显示 `--`，
//! 不伪造零值。CPU/GPU 温度及 GPU 在对应数据缺失时由 [`is_available`] 判定为不可用
//! 并整行隐藏，因此不会绘制占位符。

use trafficview_core::{
    config::RateUnit,
    format::{format_bit_rate, format_rate},
    Snapshot,
};

pub const BUILTIN_IDS: [&str; 7] = [
    "net_up", "net_down", "cpu", "mem", "cpu_temp", "gpu", "gpu_temp",
];

pub fn is_known_id(id: &str) -> bool {
    BUILTIN_IDS.contains(&id)
        || id
            .strip_prefix("plugin.")
            .is_some_and(|rest| !rest.trim().is_empty())
}

/// 显示项当前是否有可绘制数据；可选数据缺失时整行隐藏。
pub fn is_available(id: &str, snapshot: &Snapshot) -> bool {
    match id {
        "cpu_temp" => snapshot.cpu_temp.is_some(),
        "gpu" => snapshot.gpu.is_some(),
        "gpu_temp" => snapshot
            .gpu
            .as_ref()
            .is_some_and(|gpu| gpu.temp_celsius.is_some()),
        _ => true,
    }
}

/// 阈值比较使用的数值。速率始终以字节/秒参与阈值判断。
pub fn numeric_value(id: &str, snapshot: &Snapshot) -> Option<f64> {
    match id {
        "net_up" => Some(snapshot.net.tx_bps as f64),
        "net_down" => Some(snapshot.net.rx_bps as f64),
        "cpu" => Some(f64::from(snapshot.cpu.total_pct)),
        "mem" => Some(f64::from(snapshot.mem.used_pct)),
        "cpu_temp" => snapshot.cpu_temp.map(f64::from),
        "gpu" => snapshot.gpu.as_ref().map(|gpu| f64::from(gpu.util_pct)),
        "gpu_temp" => snapshot
            .gpu
            .as_ref()
            .and_then(|gpu| gpu.temp_celsius)
            .map(f64::from),
        other => other
            .strip_prefix("plugin.")
            .and_then(|key| snapshot.plugin.get(key))
            .and_then(|text| text.parse::<f64>().ok()),
    }
}

/// 格式化后的显示文本。模板中 `{value}` 会被替换为本项数值；
/// 额外支持 `{cpu_temp}`、`{gpu_temp}` 占位符，把温度内联到其它显示项。
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
        "cpu" | "mem" | "gpu" => format!("{value:.1}%"),
        "cpu_temp" | "gpu_temp" => format!("{value:.0}°C"),
        _ => format!("{value:.1}"),
    };
    let template = if format.trim().is_empty() {
        "{value}"
    } else {
        format
    };
    if let Some(expanded) = expand_temp_template(template, &text, snapshot) {
        return expanded;
    }
    template.replace("{value}", &text)
}

/// 模板包含 `{cpu_temp}` 或 `{gpu_temp}` 时，把对应温度内联到本项数值，
/// 用于 “CPU 14%  70°C” / “GPU 12%  55°C” 这类显示；有读数时原样保留模板空白。
/// 任一被引用的温度无读数时温度段为空，此时压缩空白避免留下多余间距。
/// 模板不含温度占位符时返回 `None`，调用方保持原有 `{value}` 替换逻辑。
pub fn expand_temp_template(format: &str, value: &str, snapshot: &Snapshot) -> Option<String> {
    let has_cpu = format.contains("{cpu_temp}");
    let has_gpu = format.contains("{gpu_temp}");
    if !has_cpu && !has_gpu {
        return None;
    }
    let cpu_temp = snapshot.cpu_temp.map(|celsius| format!("{celsius:.0}°C"));
    let gpu_temp = snapshot
        .gpu
        .as_ref()
        .and_then(|gpu| gpu.temp_celsius)
        .map(|celsius| format!("{celsius:.0}°C"));
    let expanded = format
        .replace("{value}", value)
        .replace("{cpu_temp}", cpu_temp.as_deref().unwrap_or_default())
        .replace("{gpu_temp}", gpu_temp.as_deref().unwrap_or_default());
    if (!has_cpu || cpu_temp.is_some()) && (!has_gpu || gpu_temp.is_some()) {
        return Some(expanded);
    }
    Some(expanded.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn format_rate_with_units(bytes_per_sec: u64, units: RateUnit) -> String {
    match units {
        RateUnit::Bytes => format_rate(bytes_per_sec),
        RateUnit::Bits => format_bit_rate(bytes_per_sec),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trafficview_core::{config::RateUnit, CpuSample, GpuSample, MemSample, NetSample};

    fn snapshot() -> Snapshot {
        Snapshot::new(
            0,
            NetSample {
                iface: "eth0".into(),
                rx_bps: 0,
                tx_bps: 0,
                rx_total: 0,
                tx_total: 0,
            },
            CpuSample {
                total_pct: 0.0,
                per_core: vec![],
                freq_mhz: None,
                load: None,
            },
            MemSample {
                used_pct: 0.0,
                total: 0,
                available: 0,
                swap_pct: 0.0,
            },
        )
    }

    fn snapshot_with_gpu(temp_celsius: Option<f32>) -> Snapshot {
        let mut snapshot = snapshot();
        snapshot.gpu = Some(GpuSample {
            name: "AMD GPU".into(),
            util_pct: 12.0,
            mem_used: 0,
            mem_total: 0,
            freq_mhz: None,
            temp_celsius,
            stale: false,
            observed_at_ms: None,
        });
        snapshot
    }

    #[test]
    fn gpu_items_availability_follows_gpu_presence() {
        let snapshot = snapshot();
        assert!(!is_available("gpu", &snapshot));
        assert!(!is_available("gpu_temp", &snapshot));
        assert!(is_available("cpu", &snapshot));

        let snapshot = snapshot_with_gpu(None);
        assert!(is_available("gpu", &snapshot));
        assert!(!is_available("gpu_temp", &snapshot));

        let snapshot = snapshot_with_gpu(Some(55.0));
        assert!(is_available("gpu_temp", &snapshot));
    }

    #[test]
    fn gpu_temp_template_inlines_and_collapses_whitespace() {
        let snapshot = snapshot_with_gpu(Some(55.0));
        assert_eq!(
            expand_temp_template("{value}  {gpu_temp}", "12%", &snapshot).as_deref(),
            Some("12%  55°C")
        );

        let snapshot = snapshot_with_gpu(None);
        assert_eq!(
            expand_temp_template("{value}  {gpu_temp}", "12%", &snapshot).as_deref(),
            Some("12%")
        );
    }

    #[test]
    fn gpu_display_values_and_skin_skip_missing_gpu() {
        let snapshot = snapshot_with_gpu(Some(55.0));
        assert_eq!(
            display_value("gpu", &snapshot, RateUnit::Bytes, "{value}"),
            "12.0%"
        );
        assert_eq!(
            display_value("gpu_temp", &snapshot, RateUnit::Bytes, "{value}"),
            "55°C"
        );
        assert_eq!(
            display_value("gpu", &snapshot, RateUnit::Bytes, "{value}  {gpu_temp}"),
            "12.0%  55°C"
        );
    }
}
