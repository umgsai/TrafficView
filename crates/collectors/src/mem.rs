use std::{collections::HashMap, fs, io, path::Path};
use trafficview_core::MemSample;

pub fn parse_meminfo(input: &str) -> Option<MemSample> {
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
    let available = *values.get("MemAvailable")?;
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
    let text = fs::read_to_string(root.join("meminfo"))?;
    parse_meminfo(&text)
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
}
