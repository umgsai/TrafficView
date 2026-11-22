use std::time::Duration;

pub fn format_rate(bytes_per_sec: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes_per_sec < 1024 {
        return format!("{bytes_per_sec} B/s");
    }
    let mut value = bytes_per_sec as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}/s", UNITS[unit])
}

/// 按比特每秒格式化（1024 进制）。输入仍是字节每秒，先乘 8 再缩写。
pub fn format_bit_rate(bytes_per_sec: u64) -> String {
    const UNITS: [&str; 5] = ["b", "Kb", "Mb", "Gb", "Tb"];
    let bits = bytes_per_sec.saturating_mul(8);
    if bits < 1024 {
        return format!("{bits} b/s");
    }
    let mut value = bits as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}/s", UNITS[unit])
}

pub fn counter_rate(previous: u64, current: u64, elapsed: Duration) -> Option<u64> {
    if elapsed.is_zero() || elapsed > Duration::from_secs(5) || current < previous {
        return None;
    }
    Some(((current - previous) as f64 / elapsed.as_secs_f64()).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn formats_binary_rates() {
        assert_eq!(format_rate(0), "0 B/s");
        assert_eq!(format_rate(1023), "1023 B/s");
        assert_eq!(format_rate(1024), "1.0 KB/s");
        assert_eq!(format_rate(1_572_864), "1.5 MB/s");
    }

    #[test]
    fn formats_optional_bit_rates_with_binary_prefixes() {
        assert_eq!(format_bit_rate(0), "0 b/s");
        assert_eq!(format_bit_rate(128), "1.0 Kb/s");
        assert_eq!(format_bit_rate(1024), "8.0 Kb/s");
        assert_eq!(format_bit_rate(1_572_864), "12.0 Mb/s");
    }

    #[test]
    fn rejects_invalid_counter_deltas() {
        assert_eq!(counter_rate(100, 90, Duration::from_secs(1)), None);
        assert_eq!(counter_rate(100, 200, Duration::ZERO), None);
        assert_eq!(counter_rate(100, 200, Duration::from_secs(6)), None);
        assert_eq!(
            counter_rate(100, 350, Duration::from_millis(500)),
            Some(500)
        );
    }
}
