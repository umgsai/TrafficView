use std::{fs, path::Path};

use trafficview_core::TempSample;

/// 扫描 `/sys/class/thermal/thermal_zone*`，label 记为 `thermal/<type>`。
/// 非法读数、非有限值以及 -50–150°C 之外的值被丢弃。
pub fn read_thermal_temps(root: &Path) -> Vec<TempSample> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut temps = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        let Some(kind) = read_trimmed(&dir.join("type")) else {
            continue;
        };
        let Ok(raw) = fs::read_to_string(dir.join("temp")) else {
            continue;
        };
        let Ok(millidegrees) = raw.trim().parse::<i64>() else {
            continue;
        };
        let celsius = millidegrees as f32 / 1000.0;
        if !celsius.is_finite() || !(-50.0..=150.0).contains(&celsius) {
            continue;
        }
        temps.push(TempSample {
            label: format!("thermal/{kind}"),
            celsius,
        });
    }
    temps.sort_by(|a, b| a.label.cmp(&b.label));
    temps
}

/// hwmon 优先；type 已被某个 hwmon chip 覆盖的 zone 丢弃，避免 `acpitz` 等重复。
pub fn merge_thermal_temps(hwmon: Vec<TempSample>, zones: Vec<TempSample>) -> Vec<TempSample> {
    let chips: Vec<String> = hwmon
        .iter()
        .map(|temp| chip_name(&temp.label).to_ascii_lowercase())
        .collect();
    let mut merged = hwmon;
    for zone in zones {
        let kind = zone
            .label
            .strip_prefix("thermal/")
            .unwrap_or(&zone.label)
            .to_ascii_lowercase();
        if chips.iter().any(|chip| chip == &kind) {
            continue;
        }
        merged.push(zone);
    }
    merged.sort_by(|a, b| a.label.cmp(&b.label));
    merged
}

fn chip_name(label: &str) -> &str {
    label.split('/').next().unwrap_or(label)
}

fn read_trimmed(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn write(path: &Path, value: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value).unwrap();
    }

    #[test]
    fn reads_thermal_zones_and_rejects_invalid_readings() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("thermal_zone0/type"), "cpu_thermal\n");
        write(&root.join("thermal_zone0/temp"), "52000\n");
        write(&root.join("thermal_zone1/type"), "acpitz\n");
        write(&root.join("thermal_zone1/temp"), "oops\n");
        write(&root.join("thermal_zone2/type"), "soc_thermal\n");
        write(&root.join("thermal_zone2/temp"), "200000\n");

        let temps = read_thermal_temps(root);
        assert_eq!(temps.len(), 1);
        assert_eq!(temps[0].label, "thermal/cpu_thermal");
        assert_eq!(temps[0].celsius, 52.0);
    }

    #[test]
    fn zones_covered_by_hwmon_chips_are_dropped() {
        let hwmon = vec![
            TempSample {
                label: "acpitz".into(),
                celsius: 65.0,
            },
            TempSample {
                label: "k10temp/Tctl".into(),
                celsius: 66.0,
            },
        ];
        let zones = vec![
            TempSample {
                label: "thermal/acpitz".into(),
                celsius: 65.0,
            },
            TempSample {
                label: "thermal/cpu_thermal".into(),
                celsius: 52.0,
            },
        ];

        let merged = merge_thermal_temps(hwmon, zones);
        assert_eq!(
            merged
                .iter()
                .map(|temp| temp.label.as_str())
                .collect::<Vec<_>>(),
            ["acpitz", "k10temp/Tctl", "thermal/cpu_thermal"]
        );
    }
}
