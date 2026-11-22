use std::{fs, path::Path};

use trafficview_core::BatterySample;

/// 读取全部 `BAT*`，按满容量加权；无电池时返回 `None`。
/// 同时存在充电与放电电池时状态记为 `Mixed`。
pub fn read_battery(root: &Path) -> Option<BatterySample> {
    let entries = fs::read_dir(root).ok()?;
    let mut total_full = 0u64;
    let mut total_now = 0u64;
    let mut statuses: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("BAT") || !dir.is_dir() {
            continue;
        }
        let (full_key, now_key) = if read_number(&dir.join("energy_full")).is_some() {
            ("energy_full", "energy_now")
        } else {
            ("charge_full", "charge_now")
        };
        let (Some(full), Some(now)) = (
            read_number(&dir.join(full_key)),
            read_number(&dir.join(now_key)),
        ) else {
            continue;
        };
        if full == 0 {
            continue;
        }
        total_full += full;
        total_now += now.min(full);
        statuses.push(read_trimmed(&dir.join("status")).unwrap_or_else(|| "Unknown".into()));
    }
    if total_full == 0 {
        return None;
    }
    let status = if statuses.windows(2).all(|pair| pair[0] == pair[1]) {
        statuses
            .first()
            .cloned()
            .unwrap_or_else(|| "Unknown".into())
    } else {
        "Mixed".into()
    };
    Some(BatterySample {
        percent: 100.0 * total_now as f32 / total_full as f32,
        status,
    })
}

fn read_number(path: &Path) -> Option<u64> {
    read_trimmed(path)?.parse().ok()
}

fn read_trimmed(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn battery(root: &Path, name: &str, full_key: &str, full: u64, now: u64, status: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(full_key), format!("{full}\n")).unwrap();
        fs::write(
            dir.join(full_key.replace("full", "now")),
            format!("{now}\n"),
        )
        .unwrap();
        fs::write(dir.join("status"), format!("{status}\n")).unwrap();
    }

    #[test]
    fn multiple_batteries_are_weighted_by_full_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        battery(
            root,
            "BAT0",
            "energy_full",
            50_000_000,
            25_000_000,
            "Charging",
        );
        battery(
            root,
            "BAT1",
            "energy_full",
            25_000_000,
            10_000_000,
            "Discharging",
        );
        fs::create_dir_all(root.join("AC")).unwrap();

        let sample = read_battery(root).expect("two batteries");
        assert!((sample.percent - 46.666_668).abs() < 0.01);
        assert_eq!(sample.status, "Mixed");
    }

    #[test]
    fn charge_counters_are_supported_and_absence_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        battery(
            root,
            "BAT0",
            "charge_full",
            4_000_000,
            3_000_000,
            "Discharging",
        );
        let sample = read_battery(root).expect("one battery");
        assert!((sample.percent - 75.0).abs() < 0.01);
        assert_eq!(sample.status, "Discharging");

        let empty = tempfile::tempdir().unwrap();
        assert_eq!(read_battery(empty.path()), None);
    }
}
