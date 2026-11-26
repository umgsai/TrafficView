use crate::TempSample;

/// 选出代表 CPU 的温度。`sensor` 非空时先按 label 精确匹配（忽略大小写），再按子串匹配，
/// 未命中回退内置规则。子串匹配取 `temps` 中首个包含覆盖值的项，顺序由采集层按 label 排序决定；
/// 因此只写 chip 名（如 `k10temp`）会按 label 排序命中第一个包含该子串的项
/// （可能是 `Tccd1`、`Tctl` 等），建议写完整 label（如 `k10temp/Tdie`）。
pub fn cpu_temperature(temps: &[TempSample], sensor: Option<&str>) -> Option<f32> {
    if let Some(wanted) = sensor.map(str::trim).filter(|value| !value.is_empty()) {
        if let Some(temp) = temps
            .iter()
            .find(|temp| temp.label.eq_ignore_ascii_case(wanted))
        {
            return Some(temp.celsius);
        }
        let wanted = wanted.to_ascii_lowercase();
        if let Some(temp) = temps
            .iter()
            .find(|temp| temp.label.to_ascii_lowercase().contains(&wanted))
        {
            return Some(temp.celsius);
        }
    }

    let find = |chip: &str, label_part: Option<&str>| {
        temps.iter().find_map(|temp| {
            let (name, label) = temp
                .label
                .split_once('/')
                .unwrap_or((temp.label.as_str(), ""));
            if !name.eq_ignore_ascii_case(chip) && !label.eq_ignore_ascii_case(chip) {
                return None;
            }
            match label_part {
                Some(part) if !label.to_ascii_lowercase().contains(part) => None,
                _ => Some(temp.celsius),
            }
        })
    };

    find("coretemp", Some("package"))
        .or_else(|| find("k10temp", Some("tdie")))
        .or_else(|| find("k10temp", Some("tctl")))
        .or_else(|| find("zenpower", Some("tdie")))
        .or_else(|| find("zenpower", Some("tctl")))
        .or_else(|| find("x86_pkg_temp", None))
        .or_else(|| find("cpu_thermal", None))
        .or_else(|| find("cpu-thermal", None))
        .or_else(|| find("soc_thermal", None))
        .or_else(|| find("soc-thermal", None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(label: &str, celsius: f32) -> TempSample {
        TempSample {
            label: label.into(),
            celsius,
        }
    }

    #[test]
    fn prefers_intel_package_and_amd_tdie() {
        let intel = vec![
            temp("coretemp/Core 0", 51.0),
            temp("coretemp/Package id 0", 55.0),
        ];
        assert_eq!(cpu_temperature(&intel, None), Some(55.0));

        let amd = vec![temp("k10temp/Tctl", 66.0), temp("k10temp/Tdie", 62.0)];
        assert_eq!(cpu_temperature(&amd, None), Some(62.0));

        let tctl_only = vec![temp("k10temp/Tctl", 66.0)];
        assert_eq!(cpu_temperature(&tctl_only, None), Some(66.0));
    }

    #[test]
    fn matches_thermal_zone_and_package_chips() {
        let arm = vec![temp("thermal/cpu_thermal", 52.0)];
        assert_eq!(cpu_temperature(&arm, None), Some(52.0));

        let x86 = vec![temp("x86_pkg_temp", 48.0)];
        assert_eq!(cpu_temperature(&x86, None), Some(48.0));

        let zen = vec![temp("zenpower/Tdie", 47.0)];
        assert_eq!(cpu_temperature(&zen, None), Some(47.0));
    }

    #[test]
    fn override_wins_and_falls_back_when_missing() {
        let temps = vec![temp("nvme/Composite", 40.0), temp("k10temp/Tctl", 66.0)];
        assert_eq!(cpu_temperature(&temps, Some("nvme/Composite")), Some(40.0));
        assert_eq!(cpu_temperature(&temps, Some("NVME")), Some(40.0));
        assert_eq!(cpu_temperature(&temps, Some("missing")), Some(66.0));
        assert_eq!(cpu_temperature(&temps, Some("  ")), Some(66.0));
    }

    #[test]
    fn non_cpu_sensors_never_fall_back_to_other_temperatures() {
        let temps = vec![
            temp("nvme/Composite", 40.0),
            temp("amdgpu/edge", 57.0),
            temp("acpitz", 65.0),
            temp("iwlwifi_1", 41.0),
        ];
        assert_eq!(cpu_temperature(&temps, None), None);
    }
}
