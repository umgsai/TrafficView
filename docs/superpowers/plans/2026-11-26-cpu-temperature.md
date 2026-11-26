# CPU 温度监控 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让悬浮窗显示 CPU 温度（`cpu_temp` 显示项），数据来自 hwmon 与 thermal_zone，按固定规则识别 CPU 传感器并支持配置覆盖，无读数时整行隐藏。

**Architecture:** 采集层合并 hwmon 与 `/sys/class/thermal` 并去重；daemon 在采样时按 `overlay.cpu_temp_sensor` 解析出 `Snapshot.cpu_temp`；渲染层把 `cpu_temp` 作为第五种内置显示项，值为 `None` 时跳过该项；D-Bus 以可选顶层键 `cpu_temp` 输出。

**Tech Stack:** Rust 2021、`tempfile`（测试）、现有 `trafficview-core` / `trafficview-collectors` / `trafficview-daemon` / `trafficview-render` crate。

**设计文档:** `docs/superpowers/specs/2026-11-26-cpu-temperature-design.md`

---

## 文件结构

- Create: `crates/collectors/src/thermal.rs` — thermal_zone 读取与 hwmon 合并去重。
- Modify: `crates/collectors/src/lib.rs` — 导出 `thermal` 模块。
- Create: `crates/core/src/temp.rs` — CPU 传感器识别规则。
- Modify: `crates/core/src/lib.rs` — 导出 `temp` 模块、`Snapshot.cpu_temp` 字段。
- Modify: `crates/core/src/config.rs` — `OverlayConfig.cpu_temp_sensor`、默认 `cpu_temp` 显示项。
- Modify: `crates/daemon/src/service.rs` — 合并温度来源、解析 `cpu_temp`。
- Modify: `crates/daemon/src/dbus.rs` — `cpu_temp` 的可选读写与测试。
- Modify: `crates/render/src/value.rs` — `cpu_temp` 的取值、格式化与可用性。
- Modify: `crates/render/src/lib.rs` — 布局/皮肤跳过不可用项、`value_text`、测试。
- Modify: `crates/render/benches/render.rs` — 补 `cpu_temp: None` 字段。
- Modify: `crates/cli/src/main.rs` — `config set overlay.cpu_temp_sensor`。
- Modify: `docs/dbus-v1.md`、`README.md` — 契约与配置说明。

---

### Task 1: thermal_zone 采集与合并

**Files:**
- Create: `crates/collectors/src/thermal.rs`
- Modify: `crates/collectors/src/lib.rs`

- [ ] **Step 1: 写失败测试**

在 `crates/collectors/src/thermal.rs` 写入模块与测试（实现留空，先让编译通过以便看到断言失败）：

```rust
use std::{fs, path::Path};

use trafficview_core::TempSample;

/// 扫描 `/sys/class/thermal/thermal_zone*`，label 记为 `thermal/<type>`。
pub fn read_thermal_temps(_root: &Path) -> Vec<TempSample> {
    Vec::new()
}

/// hwmon 优先；type 已被某个 hwmon chip 覆盖的 zone 丢弃。
pub fn merge_thermal_temps(hwmon: Vec<TempSample>, zones: Vec<TempSample>) -> Vec<TempSample> {
    hwmon.into_iter().chain(zones).collect()
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
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p trafficview-collectors thermal::`

Expected: 两个测试 FAIL（`read_thermal_temps` 返回空、合并未去重）。

- [ ] **Step 3: 实现采集与合并**

在 `crates/collectors/src/thermal.rs` 中**保留 Step 1 的测试模块**，只把测试模块之前的实现替换为：

```rust
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
```

- [ ] **Step 4: 导出模块并运行测试**

在 `crates/collectors/src/lib.rs` 的模块列表中按字母序插入：

```rust
pub mod thermal;
```

Run: `cargo test -p trafficview-collectors thermal::`

Expected: 2 passed。

- [ ] **Step 5: 提交**

```bash
git add crates/collectors/src/thermal.rs crates/collectors/src/lib.rs
git commit -m "feat(collectors): 增加 thermal_zone 温度来源并与 hwmon 去重"
```

---

### Task 2: CPU 传感器识别规则

**Files:**
- Create: `crates/core/src/temp.rs`
- Modify: `crates/core/src/lib.rs`

- [ ] **Step 1: 写失败测试**

创建 `crates/core/src/temp.rs`：

```rust
use crate::TempSample;

pub fn cpu_temperature(_temps: &[TempSample], _sensor: Option<&str>) -> Option<f32> {
    None
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

        let amd = vec![
            temp("k10temp/Tctl", 66.0),
            temp("k10temp/Tdie", 62.0),
        ];
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
        let temps = vec![
            temp("nvme/Composite", 40.0),
            temp("k10temp/Tctl", 66.0),
        ];
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
```

- [ ] **Step 2: 运行测试确认失败**

在 `crates/core/src/lib.rs` 顶部加入 `pub mod temp;`，然后：

Run: `cargo test -p trafficview-core temp::`

Expected: 4 个测试 FAIL（恒返回 `None`）。

- [ ] **Step 3: 实现识别规则**

把 `crates/core/src/temp.rs` 顶部替换为：

```rust
use crate::TempSample;

/// 选出代表 CPU 的温度。`sensor` 非空时先按 label 精确/子串匹配，未命中回退内置规则。
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
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p trafficview-core temp::`

Expected: 4 passed。

- [ ] **Step 5: 提交**

```bash
git add crates/core/src/temp.rs crates/core/src/lib.rs
git commit -m "feat(core): 增加 CPU 温度传感器识别规则"
```

---

### Task 3: 快照字段与配置项

**Files:**
- Modify: `crates/core/src/lib.rs`（`Snapshot` 结构体与 `Snapshot::new`）
- Modify: `crates/core/src/config.rs`（`OverlayConfig`）
- Modify: `crates/daemon/src/dbus.rs:709`（结构体字面量补 `cpu_temp: None`）
- Modify: `crates/render/benches/render.rs:11`（结构体字面量补 `cpu_temp: None`）

- [ ] **Step 1: 写失败测试**

在 `crates/core/src/lib.rs` 的 `constructor_keeps_optional_metrics_unset` 测试末尾追加：

```rust
        assert_eq!(snapshot.cpu_temp, None);
```

在 `crates/core/src/config.rs` 的测试模块追加：

```rust
    #[test]
    fn overlay_defaults_to_automatic_cpu_sensor() {
        let cfg = Config::default();
        assert_eq!(cfg.overlay.cpu_temp_sensor, "");
        assert!(cfg
            .overlay
            .items
            .iter()
            .any(|item| item.id == "cpu_temp"));
    }

    #[test]
    fn legacy_overlay_config_without_cpu_sensor_still_loads() {
        let text = r#"
            [overlay]
            opacity = 0.7
        "#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.overlay.cpu_temp_sensor, "");
        assert!(cfg.overlay.items.iter().any(|item| item.id == "cpu_temp"));
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p trafficview-core overlay_defaults_to_automatic_cpu_sensor -- --nocapture`

Expected: FAIL，`cpu_temp_sensor` 字段不存在（编译错误）。

- [ ] **Step 3: 增加字段与默认项**

在 `crates/core/src/lib.rs` 的 `Snapshot` 中，`temps` 之前加入：

```rust
    /// CPU 温度（摄氏度）；无可用传感器时为 `None`。
    #[serde(default)]
    pub cpu_temp: Option<f32>,
```

在 `Snapshot::new` 的 `temps: Vec::new(),` 前加入：

```rust
            cpu_temp: None,
```

在 `crates/core/src/config.rs` 的 `OverlayConfig` 中，`items` 之前加入：

```rust
    /// CPU 温度传感器 label；空字符串按内置规则自动识别。
    #[serde(default)]
    pub cpu_temp_sensor: String,
```

在 `OverlayConfig::default()` 的 `items: default_items(),` 前加入：

```rust
            cpu_temp_sensor: String::new(),
```

在 `default_items()` 的 `mem` 项之后追加：

```rust
        DisplayItem {
            id: "cpu_temp".into(),
            label: "TEMP".into(),
            unit: "°C".into(),
            format: "{value}".into(),
            color: "#FFA94D".into(),
            visible: true,
        },
```

把 `crates/core/src/config.rs` 中 `toml_round_trip_preserves_display_items` 的期望数组改为：

```rust
            ["net_up", "net_down", "cpu", "mem", "cpu_temp"]
```

把 `crates/daemon/src/dbus.rs` 中 `Ok(Snapshot { ... })` 字面量补上字段（放在 `temps,` 之前）：

```rust
        cpu_temp: None,
```

把 `crates/render/benches/render.rs` 的 `Snapshot { ... }` 字面量补上字段（放在 `temps: ...` 之前或 `disk: None,` 之后均可）：

```rust
        cpu_temp: None,
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p trafficview-core && cargo check --workspace`

Expected: 全部 PASS，工作区可编译。

- [ ] **Step 5: 提交**

```bash
git add crates/core/src/lib.rs crates/core/src/config.rs crates/daemon/src/dbus.rs crates/render/benches/render.rs
git commit -m "feat(core): 增加 CPU 温度快照字段与传感器配置"
```

---

### Task 4: 渲染层支持 cpu_temp

**Files:**
- Modify: `crates/render/src/value.rs`
- Modify: `crates/render/src/lib.rs`

- [ ] **Step 1: 写失败测试**

在 `crates/render/src/lib.rs` 测试模块追加：

```rust
    #[test]
    fn cpu_temp_item_is_hidden_until_a_reading_exists() {
        let items = Config::default().overlay.items;
        let mut snapshot = snapshot();

        let without = layout(&items, &snapshot, 96);
        assert!(!without.items.iter().any(|item| item.id == "cpu_temp"));

        snapshot.cpu_temp = Some(66.4);
        let with = layout(&items, &snapshot, 96);
        let item = with
            .items
            .iter()
            .find(|item| item.id == "cpu_temp")
            .expect("cpu_temp item");
        assert_eq!(item.text, "TEMP 66°C");
        assert!(with.height > without.height);
    }

    #[test]
    fn cpu_temp_display_value_uses_integer_celsius() {
        let mut snapshot = snapshot();
        snapshot.cpu_temp = Some(66.4);
        assert_eq!(
            display_value("cpu_temp", &snapshot, trafficview_core::config::RateUnit::Bytes, "{value}"),
            "66°C"
        );
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p trafficview-render cpu_temp_`

Expected: FAIL（`cpu_temp` 不在已知 id，布局仍显示空文本项）。

- [ ] **Step 3: 实现取值、格式化与隐藏**

把 `crates/render/src/value.rs` 改为：

```rust
//! `DisplayItem` 取值、单位格式与模板替换。
//!
//! 该模块是皮肤与内置布局共用的唯一取值注册表；未采集的可选值显式显示 `--`，
//! 不伪造零值。

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
```

在 `crates/render/src/lib.rs`：

1. 导出行加入 `is_available`：

```rust
pub use value::{display_value, is_available, is_known_id, numeric_value};
```

2. `layout()` 的可见项过滤改为：

```rust
    let visible_items: Vec<&DisplayItem> = items
        .iter()
        .filter(|item| item.visible && value::is_available(&item.id, snapshot))
        .collect();
```

3. `value_text()` 增加分支：

```rust
        "cpu_temp" => snapshot
            .cpu_temp
            .map(|celsius| format!("{celsius:.0}°C"))
            .unwrap_or_default(),
```

4. 皮肤渲染循环 `for item in &self.skin.items {` 之后、取 `item.rect` 之前加入：

```rust
            if !value::is_available(&item.id, snapshot) {
                continue;
            }
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p trafficview-render && cargo test -p trafficview-core`

Expected: 全部 PASS；默认布局在无温度读数时仍是 4 项，有读数时 5 项。

- [ ] **Step 5: 提交**

```bash
git add crates/render/src/value.rs crates/render/src/lib.rs
git commit -m "feat(render): 悬浮窗支持 CPU 温度显示项"
```

---

### Task 5: daemon 解析与 D-Bus 输出

**Files:**
- Modify: `crates/daemon/src/service.rs:18-31`、`:283-293`、`:379-381`、`:528-530`
- Modify: `crates/daemon/src/dbus.rs:410-420`、`:568-570`、`:700-725`、`:1250-1270`
- Modify: `docs/dbus-v1.md:53-66`

- [ ] **Step 1: 写失败测试**

在 `crates/daemon/src/dbus.rs` 测试模块追加：

```rust
    #[test]
    fn cpu_temp_is_optional_and_round_trips() {
        let mut snapshot = snapshot();
        assert!(!snapshot_to_dbus(&snapshot).contains_key("cpu_temp"));

        snapshot.cpu_temp = Some(66.5);
        let dict = snapshot_to_dbus(&snapshot);
        assert!(dict.contains_key("cpu_temp"));

        let decoded = snapshot_from_dbus(&dict).unwrap();
        assert_eq!(decoded.cpu_temp, Some(66.5));
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p trafficview-daemon cpu_temp_is_optional_and_round_trips`

Expected: FAIL，`cpu_temp` 键未写入。

- [ ] **Step 3: 实现采集、解析与序列化**

`crates/daemon/src/service.rs`：

1. 导入：

```rust
use trafficview_collectors::{
    cpu::{read_freq_mhz, read_loadavg, ProcCpu},
    disk::{read_filesystems, DiskSampler},
    gpu::GpuSampler,
    hwmon::read_temps,
    mem::read_mem_with_mode,
    net::NetSampler,
    netlink::NetlinkWatcher,
    power::read_battery,
    thermal::{merge_thermal_temps, read_thermal_temps},
    uptime::UptimeState,
};
```

2. `trafficview_core` 导入加入 `temp::cpu_temperature`：

```rust
use trafficview_core::{
    clock::monotonic_ms,
    config::{Config, ConfigStore},
    smoothing::Smoother,
    temp::cpu_temperature,
    ColorScheme, CpuSample, InterfaceInfo, NetSample, Snapshot,
};
```

3. 采样循环中的元组解构增加一项（`state.config.network_smoothing` 之后）：

```rust
                state.config.overlay.cpu_temp_sensor.clone(),
```

并把左侧绑定改为：

```rust
        let (
            interval_ms,
            paused,
            configured_iface,
            include_cache,
            whitelist,
            blacklist,
            smoothing,
            cpu_temp_sensor,
        ) = {
```

4. 温度采集替换为：

```rust
        let temps = merge_thermal_temps(
            read_temps(&sys_class.join("hwmon")),
            read_thermal_temps(&sys_class.join("thermal")),
        );
```

5. 状态原因改为：

```rust
                    reason: if temps.is_empty() {
                        "no hwmon or thermal sensors".into()
                    } else {
                        String::new()
                    },
```

6. `snapshot.temps = temps;` 之后加入：

```rust
        snapshot.cpu_temp = cpu_temperature(&snapshot.temps, Some(cpu_temp_sensor.as_str()));
```

`crates/daemon/src/dbus.rs`：

1. `snapshot_to_dbus` 中 `temps` 键之后加入：

```rust
    if let Some(cpu_temp) = snapshot.cpu_temp {
        top.insert(
            "cpu_temp".into(),
            owned(Value::from(f64::from(cpu_temp))),
        );
    }
```

2. `snapshot_from_dbus` 中 `temps` 解析之后加入：

```rust
    let cpu_temp = dict
        .get("cpu_temp")
        .map(|value| -> anyhow::Result<f32> {
            Ok(f64::try_from(value.try_clone()?)? as f32)
        })
        .transpose()?;
```

3. 末尾 `Ok(Snapshot { ... })` 的 `temps,` 之前加入：

```rust
        cpu_temp,
```

`docs/dbus-v1.md`：

- 可选键列表加入 `cpu_temp`：

```markdown
顶层必需键：`ts_ms`、`net`、`cpu`、`mem`。`cpu_temp`、`disk`、`gpu`、`filesystems`、
`battery`、`uptime_s` 是可选键，未采集时直接省略（D-Bus variant 没有 null 语义）；
`temps` 为空 `av`，`plugin` 为空 `a{ss}`。
```

- 字段清单在 `temps` 之前插入：

```text
cpu_temp    d      [可选] 代表 CPU 的温度（摄氏度），无可用传感器时省略
```

- 测试清单段落（第 92 行附近）的“可选键省略”描述保持原样，无需改动。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p trafficview-daemon cpu_temp && cargo test -p trafficview-daemon dbus`

Expected: 全部 PASS。

- [ ] **Step 5: 提交**

```bash
git add crates/daemon/src/service.rs crates/daemon/src/dbus.rs docs/dbus-v1.md
git commit -m "feat(daemon): 解析 CPU 温度并通过 D-Bus 输出 cpu_temp"
```

---

### Task 6: CLI 配置命令与文档

**Files:**
- Modify: `crates/cli/src/main.rs:662-684`
- Modify: `README.md`（当前能力、配置表）

- [ ] **Step 1: 写失败测试**

在 `crates/cli/src/main.rs` 文件末尾追加（该文件当前没有测试模块）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_set_accepts_cpu_temp_sensor() {
        let mut config = Config::default();
        apply_setting(&mut config, "overlay.cpu_temp_sensor", " k10temp/Tctl ").unwrap();
        assert_eq!(config.overlay.cpu_temp_sensor, "k10temp/Tctl");
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p trafficview-cli config_set_accepts_cpu_temp_sensor`

Expected: FAIL，`unsupported config key: overlay.cpu_temp_sensor`。

- [ ] **Step 3: 实现设置项并更新 README**

`apply_setting` 的 `match` 在 `"memory_include_cache"` 分支之后加入：

```rust
        "overlay.cpu_temp_sensor" => {
            config.overlay.cpu_temp_sensor = value.trim().to_owned();
        }
```

`README.md`：

1. 「当前能力」第一条改为：

```markdown
- **悬浮窗**（X11）：上下行速率、CPU、内存、CPU 温度等显示项；可拖动、悬停穿透、调透明度、置顶，
  皮肤用 TOML 描述，支持阈值变色。
```

2. 「主要字段」表格在 `[overlay]` 行之前加入：

```markdown
| `overlay.cpu_temp_sensor` | CPU 温度传感器 label；留空自动识别，例如 `k10temp/Tctl` |
```

3. 「主要字段」表格之后追加说明：

```markdown
CPU 温度显示项 id 为 `cpu_temp`，默认配置已包含；检测不到 CPU 传感器时该项自动隐藏。
需要手动指定传感器时，可执行 `trafficview config set overlay.cpu_temp_sensor k10temp/Tctl`，
或在 `[overlay]` 中写 `cpu_temp_sensor = "..."`。
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p trafficview-cli config_set_accepts_cpu_temp_sensor`

Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add crates/cli/src/main.rs README.md
git commit -m "feat(cli): 支持设置 CPU 温度传感器并补充文档"
```

---

### Task 7: 全量验证

**Files:** 无新增改动（只运行检查）。

- [ ] **Step 1: 格式与静态检查**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: 无输出、退出码 0。

- [ ] **Step 2: 工作区测试**

Run:

```bash
dbus-run-session -- cargo test --workspace
```

Expected: 全部 `test result: ok`。

- [ ] **Step 3: 本机实机验证**

Run:

```bash
cargo build --release -p trafficview-cli
pkill -f '/usr/bin/trafficview overlay'
nohup ./target/release/trafficview overlay >/dev/null 2>&1 &
```

Expected：悬浮窗出现 `TEMP 66°C` 一类的第三行；`trafficview status --format json` 中包含
`"cpu_temp": <数值>`。把配置改为 `cpu_temp_sensor = "missing"` 后重启 daemon/overlay，
该项应回退到 `k10temp/Tctl`，而不是消失。

- [ ] **Step 4: 打包检查（不提交产物）**

Run:

```bash
SKIP_QUALITY_GATE=1 ./packaging/build-deb.sh
```

Expected: 生成 `dist/trafficview_<version>-1_amd64.deb`，`dpkg-deb -c` 内容与 1.0.2 一致。
版本号升级由发布流程决定，本任务不修改版本号。
