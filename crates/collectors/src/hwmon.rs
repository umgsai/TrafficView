use std::{fs, path::Path};

use trafficview_core::TempSample;

/// 扫描 `hwmon*/temp*_input`，按 `chip/label` 命名并稳定排序。
/// 非法读数、非有限值以及 -50–150°C 之外的值被丢弃。
pub fn read_temps(root: &Path) -> Vec<TempSample> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut temps = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(chip) = read_trimmed(&dir.join("name")) else {
            continue;
        };
        let Ok(files) = fs::read_dir(&dir) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            let Some(index) = name
                .strip_prefix("temp")
                .and_then(|rest| rest.strip_suffix("_input"))
            else {
                continue;
            };
            let Ok(raw) = fs::read_to_string(file.path()) else {
                continue;
            };
            let Ok(millidegrees) = raw.trim().parse::<i64>() else {
                continue;
            };
            let celsius = millidegrees as f32 / 1000.0;
            if !celsius.is_finite() || !(-50.0..=150.0).contains(&celsius) {
                continue;
            }
            let label = read_trimmed(&dir.join(format!("temp{index}_label")))
                .map(|label| format!("{chip}/{label}"))
                .unwrap_or_else(|| chip.clone());
            temps.push(TempSample { label, celsius });
        }
    }
    temps.sort_by(|a, b| a.label.cmp(&b.label));
    temps
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

    fn write(path: &Path, value: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value).unwrap();
    }

    #[test]
    fn reads_labelled_temperatures_in_stable_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("hwmon1/name"), "k10temp\n");
        write(&root.join("hwmon1/temp1_input"), "60000\n");
        write(&root.join("hwmon0/name"), "coretemp\n");
        write(&root.join("hwmon0/temp1_label"), "Package id 0\n");
        write(&root.join("hwmon0/temp1_input"), "45000\n");

        let temps = read_temps(root);
        assert_eq!(temps.len(), 2);
        assert_eq!(temps[0].label, "coretemp/Package id 0");
        assert_eq!(temps[0].celsius, 45.0);
        assert_eq!(temps[1].label, "k10temp");
        assert_eq!(temps[1].celsius, 60.0);
    }

    #[test]
    fn rejects_invalid_and_out_of_range_readings() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("hwmon0/name"), "coretemp\n");
        write(&root.join("hwmon0/temp1_input"), "oops\n");
        write(&root.join("hwmon0/temp2_input"), "-60000\n");
        write(&root.join("hwmon0/temp3_input"), "200000\n");
        write(&root.join("hwmon0/temp4_input"), "42000\n");

        let temps = read_temps(root);
        assert_eq!(temps.len(), 1);
        assert_eq!(temps[0].celsius, 42.0);
    }
}
