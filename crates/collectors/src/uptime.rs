use std::{fs, path::Path};

/// `/proc/uptime` 第一个浮点字段转为非负整秒；非法或负值返回 `None`。
pub fn parse_uptime(input: &str) -> Option<u64> {
    let value: f64 = input.split_whitespace().next()?.parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    Some(value as u64)
}

/// 记录上一次读数，时间倒退视为异常并返回 `None`。
#[derive(Default)]
pub struct UptimeState {
    previous: Option<u64>,
}

impl UptimeState {
    pub fn read(&mut self, proc_root: &Path) -> Option<u64> {
        let text = fs::read_to_string(proc_root.join("uptime")).ok()?;
        let value = parse_uptime(&text)?;
        if self.previous.is_some_and(|previous| value < previous) {
            return None;
        }
        self.previous = Some(value);
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parses_the_first_float_field_as_whole_seconds() {
        assert_eq!(parse_uptime("12345.67 999.00\n"), Some(12_345));
        assert_eq!(parse_uptime("0.00 0.00\n"), Some(0));
        assert_eq!(parse_uptime("nan 1.0\n"), None);
        assert_eq!(parse_uptime("-1 1.0\n"), None);
        assert_eq!(parse_uptime(""), None);
    }

    #[test]
    fn backwards_time_is_rejected_after_the_first_reading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uptime");
        fs::write(&path, "100.00 1.00\n").unwrap();
        let mut state = UptimeState::default();
        assert_eq!(state.read(dir.path()), Some(100));
        fs::write(&path, "90.00 1.00\n").unwrap();
        assert_eq!(state.read(dir.path()), None);
        fs::write(&path, "120.00 1.00\n").unwrap();
        assert_eq!(state.read(dir.path()), Some(120));
    }
}
