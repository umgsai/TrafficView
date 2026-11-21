use std::{collections::HashMap, fs, io, path::Path};
use trafficview_core::MemSample;

pub fn parse_meminfo(input: &str) -> Option<MemSample> {
    parse_meminfo_with_mode(input, false)
}

/// `include_cache=false` 使用 `MemTotal - MemAvailable`（与 GNOME/htop 一致）；
/// `include_cache=true` 使用 `MemTotal - MemFree`（TrafficMonitor 风格）。
pub fn parse_meminfo_with_mode(input: &str, include_cache: bool) -> Option<MemSample> {
    let values: HashMap<&str, u64> = input
        .lines()
        .filter_map(|line| {
            let (key, rest) = line.split_once(':')?;
            Some((
                key,
                rest.split_whitespace().next()?.parse::<u64>().ok()? * 1024,
            ))
        })
        .collect();
    let total = *values.get("MemTotal")?;
    let available = if include_cache {
        *values.get("MemFree")?
    } else {
        *values.get("MemAvailable")?
    };
    let swap_total = values.get("SwapTotal").copied().unwrap_or(0);
    let swap_free = values.get("SwapFree").copied().unwrap_or(0);
    if total == 0 || available > total || swap_free > swap_total {
        return None;
    }
    Some(MemSample {
        used_pct: 100.0 * (total - available) as f32 / total as f32,
        total,
        available,
        swap_pct: if swap_total == 0 {
            0.0
        } else {
            100.0 * (swap_total - swap_free) as f32 / swap_total as f32
        },
    })
}

pub fn read_mem(root: &Path) -> io::Result<MemSample> {
    read_mem_with_mode(root, false)
}

pub fn read_mem_with_mode(root: &Path, include_cache: bool) -> io::Result<MemSample> {
    let text = fs::read_to_string(root.join("meminfo"))?;
    parse_meminfo_with_mode(&text, include_cache)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid /proc/meminfo"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_mem_available_and_handles_no_swap() {
        let input = "MemTotal: 1000 kB\nMemAvailable: 250 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n";
        let sample = parse_meminfo(input).unwrap();
        assert_eq!(sample.total, 1_024_000);
        assert_eq!(sample.available, 256_000);
        assert_eq!(sample.used_pct, 75.0);
        assert_eq!(sample.swap_pct, 0.0);
    }

    #[test]
    fn include_cache_switches_to_mem_free() {
        let input = "MemTotal: 1000 kB\nMemAvailable: 250 kB\nMemFree: 100 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n";
        assert_eq!(
            parse_meminfo_with_mode(input, false).unwrap().used_pct,
            75.0
        );
        let cached = parse_meminfo_with_mode(input, true).unwrap();
        assert_eq!(cached.used_pct, 90.0);
        assert_eq!(cached.available, 102_400);
        // M0 默认口径保持不变。
        assert_eq!(parse_meminfo(input).unwrap().used_pct, 75.0);
    }
}
