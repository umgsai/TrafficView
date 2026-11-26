# GPU 使用率与温度显示 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 悬浮窗在 CPU 下面一行显示主 GPU 的使用率与内联温度（`GPU 12%  55°C`），温度归属于 `GpuSample`，无 GPU 时整行隐藏，无温度时只显示使用率。

**Architecture:** 采集层为 `GpuSample` 新增 `temp_celsius`（AMD 读 `card*/device/hwmon`，NVIDIA 用 nvidia-smi 已有温度列与 NVML `nvmlDeviceGetTemperature`）；D-Bus 的 gpu/gpus 字典增加可选键；渲染层新增 `gpu`/`gpu_temp` 内置 id，并把 `{cpu_temp}` 模板泛化为同时支持 `{gpu_temp}`；默认显示项追加一个内联温度的 `gpu` 项。

**Tech Stack:** Rust 2021、`tempfile`（测试）、现有 `trafficview-core` / `trafficview-collectors` / `trafficview-daemon` / `trafficview-render` / `trafficview-ui-x11` crate。

**设计文档:** `docs/superpowers/specs/2026-11-26-gpu-usage-temperature-design.md`

---

## 文件结构

- Modify: `crates/core/src/lib.rs` — `GpuSample.temp_celsius` 字段与 serde 缺省测试。
- Modify: `crates/collectors/src/gpu.rs` — AMD hwmon 温度、nvidia-smi 温度列、NVML 温度符号。
- Modify: `crates/daemon/src/dbus.rs` — gpu/gpus 字典的 `temp_celsius` 可选读写。
- Modify: `crates/render/src/value.rs` — `gpu`/`gpu_temp` 取值、可用性、模板泛化。
- Modify: `crates/render/src/lib.rs` — `value_text`、模板调用改名、布局测试。
- Modify: `crates/core/src/config.rs` — 默认 `gpu` 显示项。
- Modify: `crates/ui-x11/src/lib.rs` — 初始尺寸估算覆盖 `gpu`。
- Modify: `docs/dbus-v1.md`、`README.md` — 契约与配置说明。

---

### Task 1: `GpuSample.temp_celsius` 字段

**Files:**
- Modify: `crates/core/src/lib.rs`
- Modify: 所有构造 `GpuSample` 字面量的位置（见 Step 3）

- [ ] **Step 1: 写字段与失败测试**

在 `crates/core/src/lib.rs` 的 `GpuSample` 中，把：

```rust
    #[serde(default)]
    pub freq_mhz: Option<u32>,
    #[serde(default)]
    pub stale: bool,
```

改为：

```rust
    #[serde(default)]
    pub freq_mhz: Option<u32>,
    /// GPU 核心温度（摄氏度）；后端拿不到时为 `None`。
    #[serde(default)]
    pub temp_celsius: Option<f32>,
    #[serde(default)]
    pub stale: bool,
```

在 `crates/core/src/lib.rs` 的测试模块（若无则在文件末尾新建 `#[cfg(test)] mod tests`）加入：

```rust
    #[test]
    fn gpu_sample_without_temp_field_deserializes_to_none() {
        let json = r#"{"name":"AMD GPU","util_pct":12.0,"mem_used":0,"mem_total":0}"#;
        let gpu: GpuSample = serde_json::from_str(json).unwrap();
        assert_eq!(gpu.temp_celsius, None);

        let gpu = GpuSample {
            temp_celsius: Some(55.0),
            ..gpu
        };
        let text = serde_json::to_string(&gpu).unwrap();
        assert!(text.contains("temp_celsius"));
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p trafficview-core gpu_sample_without_temp_field_deserializes_to_none`

Expected: 编译 FAIL（`GpuSample` 尚无 `temp_celsius` 字段时测试引用了新字段，加上字段后其它 crate 的构造点报缺字段）。

- [ ] **Step 3: 修复所有 `GpuSample` 构造点**

Run: `cargo check --workspace --all-targets 2>&1 | grep -B2 'missing field.*temp_celsius'`

对每一处报错，在构造字面量的 `freq_mhz` 之后插入一行：

```rust
                temp_celsius: None,
```

已知位置（行号以当前代码为准）：

- `crates/collectors/src/gpu.rs`：`amd_card()`（约 46 行）、`parse_nvidia_smi()`（约 72 行）、NVML 分支（约 208 行）、`IntelScanner::sample()`（约 308 行）。
- 其余位置以编译错误列表为准（daemon / cli / ui 的测试与 mock 也可能构造 `GpuSample`）。

Task 2、Task 3 会把其中三处的 `None` 换成真实读数。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p trafficview-core && cargo check --workspace --all-targets`

Expected: 新测试 PASS，工作区编译无错误。

- [ ] **Step 5: 提交**

```bash
git add -A
git commit -m "feat(core): GpuSample 增加可选温度字段"
```

---

### Task 2: AMD hwmon 温度采集

**Files:**
- Modify: `crates/collectors/src/gpu.rs`

- [ ] **Step 1: 写失败测试**

在 `crates/collectors/src/gpu.rs` 的测试模块中加入（辅助函数 `write` 若已存在则复用）：

```rust
    fn write(path: &Path, value: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value).unwrap();
    }

    fn amd_device(root: &Path) -> PathBuf {
        let device = root.join("card0/device");
        write(&device.join("vendor"), "0x1002\n");
        write(&device.join("gpu_busy_percent"), "12\n");
        device
    }

    #[test]
    fn amd_temp_prefers_edge_and_converts_millidegrees() {
        let dir = tempfile::tempdir().unwrap();
        let device = amd_device(dir.path());
        write(&device.join("hwmon/hwmon2/temp1_label"), "junction\n");
        write(&device.join("hwmon/hwmon2/temp1_input"), "70000\n");
        write(&device.join("hwmon/hwmon2/temp2_label"), "edge\n");
        write(&device.join("hwmon/hwmon2/temp2_input"), "55000\n");

        assert_eq!(amd_temp_celsius(&device), Some(55.0));
    }

    #[test]
    fn amd_temp_falls_back_and_rejects_invalid_readings() {
        let dir = tempfile::tempdir().unwrap();
        let device = amd_device(dir.path());
        // edge 越界被丢弃，回退 junction；label 不存在时回退 mem。
        write(&device.join("hwmon/hwmon0/temp1_label"), "edge\n");
        write(&device.join("hwmon/hwmon0/temp1_input"), "200000\n");
        write(&device.join("hwmon/hwmon0/temp2_label"), "junction\n");
        write(&device.join("hwmon/hwmon0/temp2_input"), "oops\n");
        write(&device.join("hwmon/hwmon0/temp3_label"), "mem\n");
        write(&device.join("hwmon/hwmon0/temp3_input"), "62000\n");
        assert_eq!(amd_temp_celsius(&device), Some(62.0));

        let dir = tempfile::tempdir().unwrap();
        let device = amd_device(dir.path());
        assert_eq!(amd_temp_celsius(&device), None);
    }

    #[test]
    fn amd_card_carries_temperature() {
        let dir = tempfile::tempdir().unwrap();
        let device = amd_device(dir.path());
        write(&device.join("hwmon/hwmon0/temp1_label"), "edge\n");
        write(&device.join("hwmon/hwmon0/temp1_input"), "55000\n");

        let gpu = super::amd_card(&dir.path().join("card0"), 0).unwrap();
        assert_eq!(gpu.temp_celsius, Some(55.0));
        assert_eq!(gpu.util_pct, 12.0);
    }
```

同时在 `amd_card()` 之前加入占位实现，让测试能编译：

```rust
/// 从 `card*/device/hwmon/hwmon*/` 读取 GPU 温度；优先 edge，其次 junction、mem。
pub fn amd_temp_celsius(_device: &Path) -> Option<f32> {
    None
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p trafficview-collectors gpu::`

Expected: 新增 3 个测试 FAIL（温度恒为 `None`）。

- [ ] **Step 3: 实现温度读取并接入 `amd_card`**

把占位实现替换为：

```rust
/// 从 `card*/device/hwmon/hwmon*/` 读取 GPU 温度；优先 edge，其次 junction、mem。
/// 非法读数、非有限值以及 -50–150°C 之外的值被丢弃。
pub fn amd_temp_celsius(device: &Path) -> Option<f32> {
    let mut readings: std::collections::HashMap<String, f32> = std::collections::HashMap::new();
    let Ok(hwmon_root) = fs::read_dir(device.join("hwmon")) else {
        return None;
    };
    for hwmon in hwmon_root.flatten() {
        let Ok(entries) = fs::read_dir(hwmon.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(index) = name
                .strip_prefix("temp")
                .and_then(|rest| rest.strip_suffix("_label"))
            else {
                continue;
            };
            let Some(label) = read_trimmed(&entry.path()) else {
                continue;
            };
            let label = label.to_ascii_lowercase();
            if !matches!(label.as_str(), "edge" | "junction" | "mem")
                || readings.contains_key(&label)
            {
                continue;
            }
            let Some(raw) = read_trimmed(&hwmon.path().join(format!("temp{index}_input"))) else {
                continue;
            };
            let Ok(millidegrees) = raw.parse::<i64>() else {
                continue;
            };
            let celsius = millidegrees as f32 / 1000.0;
            if !celsius.is_finite() || !(-50.0..=150.0).contains(&celsius) {
                continue;
            }
            readings.insert(label, celsius);
        }
    }
    ["edge", "junction", "mem"]
        .iter()
        .find_map(|label| readings.get(*label).copied())
}
```

在 `amd_card()` 的 `GpuSample` 构造中，把 `temp_celsius: None,` 改为：

```rust
        temp_celsius: amd_temp_celsius(&device),
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p trafficview-collectors gpu::`

Expected: 全部通过。

- [ ] **Step 5: 提交**

```bash
git add crates/collectors/src/gpu.rs
git commit -m "feat(collectors): AMD 从 device/hwmon 读取 GPU 温度"
```

---

### Task 3: NVIDIA 温度（nvidia-smi 与 NVML）

**Files:**
- Modify: `crates/collectors/src/gpu.rs`

- [ ] **Step 1: 写失败测试**

在测试模块中加入：

```rust
    #[test]
    fn nvidia_smi_rows_carry_temperature() {
        let input = "0, NVIDIA GeForce RTX 3060, 12, 1024, 55\n";
        let gpus = parse_nvidia_smi(input);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].temp_celsius, Some(55.0));
    }

    #[test]
    fn nvidia_smi_invalid_temperature_becomes_none() {
        let gpus = parse_nvidia_smi("0, GPU, 12, 1024, [N/A]\n");
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].temp_celsius, None);

        let gpus = parse_nvidia_smi("0, GPU, 12, 1024, 300\n");
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].temp_celsius, None);
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p trafficview-collectors gpu::nvidia`

Expected: 2 个测试 FAIL（Task 1 把温度固定为 `None`）。

- [ ] **Step 3: 解析 nvidia-smi 温度列**

在 `parse_nvidia_smi()` 中，把 `Some(GpuSample {` 之前的校验块之后、构造之前的代码保持不动，仅将构造里的 `temp_celsius: None,` 改为：

```rust
                temp_celsius: fields[4]
                    .parse::<f32>()
                    .ok()
                    .filter(|celsius| celsius.is_finite() && (-50.0..=150.0).contains(celsius)),
```

- [ ] **Step 4: NVML 增加温度符号**

在 `read_nvml_gpus()` 中，`memory` 符号声明之后加入（可选符号，缺失时只影响温度）：

```rust
        let temperature: Option<unsafe extern "C" fn(*mut libc::c_void, u32, *mut u32) -> i32> =
            nvml_symbol(library, c"nvmlDeviceGetTemperature").ok();
```

在 NVML 的 `gpus.push(GpuSample { ... })` 之前加入：

```rust
                let temp_celsius = temperature.and_then(|get| {
                    let mut celsius = 0u32;
                    // NVML_TEMPERATURE_GPU = 0；读数为整数摄氏度。
                    (get(device, 0, &mut celsius) == 0 && celsius <= 150)
                        .then_some(celsius as f32)
                });
```

并把该构造里的 `temp_celsius: None,` 改为：

```rust
                    temp_celsius,
```

Intel 分支保持 `temp_celsius: None` 不变。

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test -p trafficview-collectors gpu:: && cargo check -p trafficview-collectors`

Expected: 全部通过，无编译错误。

- [ ] **Step 6: 提交**

```bash
git add crates/collectors/src/gpu.rs
git commit -m "feat(collectors): NVIDIA 采集 GPU 温度"
```

---

### Task 4: D-Bus `temp_celsius` 可选键

**Files:**
- Modify: `crates/daemon/src/dbus.rs`
- Modify: `docs/dbus-v1.md`

- [ ] **Step 1: 写失败测试**

在 `crates/daemon/src/dbus.rs` 的测试模块中加入（若已有构造 `GpuSample` 的辅助函数则复用）：

```rust
    #[test]
    fn gpu_temp_celsius_roundtrip_and_omission() {
        let mut snapshot = Snapshot::new();
        snapshot.gpu = Some(GpuSample {
            name: "AMD GPU".into(),
            util_pct: 12.0,
            mem_used: 0,
            mem_total: 0,
            freq_mhz: None,
            temp_celsius: Some(55.0),
            stale: false,
            observed_at_ms: None,
        });
        snapshot.gpus = vec![snapshot.gpu.clone().unwrap()];

        let dict = snapshot_to_dbus(&snapshot);
        let gpu: HashMap<String, OwnedValue> = dict
            .get("gpu")
            .unwrap()
            .try_clone()
            .unwrap()
            .try_into()
            .unwrap();
        assert!(gpu.contains_key("temp_celsius"));

        let restored = snapshot_from_dbus(&dict).unwrap();
        assert_eq!(restored.gpu.unwrap().temp_celsius, Some(55.0));
        assert_eq!(restored.gpus[0].temp_celsius, Some(55.0));

        snapshot.gpu.as_mut().unwrap().temp_celsius = None;
        snapshot.gpus[0].temp_celsius = None;
        let dict = snapshot_to_dbus(&snapshot);
        let gpu: HashMap<String, OwnedValue> = dict
            .get("gpu")
            .unwrap()
            .try_clone()
            .unwrap()
            .try_into()
            .unwrap();
        assert!(!gpu.contains_key("temp_celsius"));
        let restored = snapshot_from_dbus(&dict).unwrap();
        assert_eq!(restored.gpu.unwrap().temp_celsius, None);
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `dbus-run-session -- cargo test -p trafficview-daemon gpu_temp_celsius_roundtrip_and_omission`

Expected: FAIL（`gpu` 字典里没有 `temp_celsius` 键）。

- [ ] **Step 3: 写入可选键**

在 `gpu_value()` 中，`freq_mhz` 的可选写入之后加入：

```rust
    if let Some(temp_celsius) = sample.temp_celsius {
        map.insert("temp_celsius".into(), Value::from(f64::from(temp_celsius)));
    }
```

在 `gpu_from_dbus()` 中，`observed_at_ms` 的读取之后加入：

```rust
    let temp_celsius = dict
        .get("temp_celsius")
        .map(|value| f64::try_from(value.try_clone()?))
        .transpose()?
        .map(|value| value as f32);
```

并在返回的 `GpuSample` 构造里加入：

```rust
        temp_celsius,
```

- [ ] **Step 4: 更新文档并运行测试**

在 `docs/dbus-v1.md` 的 GPU 字段说明处补充一行：

```markdown
- `gpu` / `gpus[]` 可选键 `temp_celsius`（`d`）：GPU 核心温度（摄氏度），无读数时省略。
```

Run: `dbus-run-session -- cargo test -p trafficview-daemon`

Expected: 全部通过。

- [ ] **Step 5: 提交**

```bash
git add crates/daemon/src/dbus.rs docs/dbus-v1.md
git commit -m "feat(daemon): D-Bus GPU 字典增加可选温度键"
```

---

### Task 5: 渲染 `gpu`/`gpu_temp` 与模板泛化

**Files:**
- Modify: `crates/render/src/value.rs`
- Modify: `crates/render/src/lib.rs`
- Modify: `crates/core/src/config.rs`

- [ ] **Step 1: 写失败测试**

在 `crates/render/src/value.rs` 末尾加入测试模块：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use trafficview_core::{config::RateUnit, GpuSample};

    fn snapshot_with_gpu(temp_celsius: Option<f32>) -> Snapshot {
        let mut snapshot = Snapshot::new();
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
        let snapshot = Snapshot::new();
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
```

在 `crates/render/src/lib.rs` 的测试模块中加入：

```rust
    #[test]
    fn layout_hides_gpu_row_without_gpu_and_inlines_temp() {
        let items = vec![DisplayItem {
            id: "gpu".into(),
            label: "GPU".into(),
            unit: "%".into(),
            format: "{value}  {gpu_temp}".into(),
            color: "#B197FC".into(),
            visible: true,
        }];
        let mut snapshot = Snapshot::new();
        let layout = layout(&items, &snapshot, 96);
        assert!(layout.items.is_empty());

        snapshot.gpu = Some(GpuSample {
            name: "AMD GPU".into(),
            util_pct: 12.0,
            mem_used: 0,
            mem_total: 0,
            freq_mhz: None,
            temp_celsius: Some(55.0),
            stale: false,
            observed_at_ms: None,
        });
        let layout = layout(&items, &snapshot, 96);
        assert_eq!(layout.items.len(), 1);
        assert_eq!(layout.items[0].text, "GPU 12%  55°C");

        snapshot.gpu.as_mut().unwrap().temp_celsius = None;
        let layout = layout(&items, &snapshot, 96);
        assert_eq!(layout.items[0].text, "GPU 12%");
    }
```

若该测试模块尚未导入 `GpuSample`，在 `use` 中补充 `trafficview_core::GpuSample`。

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p trafficview-render`

Expected: 编译 FAIL（`expand_temp_template` 未定义、`gpu`/`gpu_temp` 未注册）。

- [ ] **Step 3: 扩展取值注册表**

在 `crates/render/src/value.rs` 中：

把 `BUILTIN_IDS` 改为：

```rust
pub const BUILTIN_IDS: [&str; 7] = [
    "net_up", "net_down", "cpu", "mem", "cpu_temp", "gpu", "gpu_temp",
];
```

把 `is_available` 改为：

```rust
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
```

在 `numeric_value` 的 match 中、`"cpu_temp"` 分支之后加入：

```rust
        "gpu" => snapshot.gpu.as_ref().map(|gpu| f64::from(gpu.util_pct)),
        "gpu_temp" => snapshot
            .gpu
            .as_ref()
            .and_then(|gpu| gpu.temp_celsius)
            .map(f64::from),
```

在 `display_value` 的文本 match 中，把：

```rust
        "cpu" | "mem" => format!("{value:.1}%"),
        "cpu_temp" => format!("{value:.0}°C"),
```

改为：

```rust
        "cpu" | "mem" | "gpu" => format!("{value:.1}%"),
        "cpu_temp" | "gpu_temp" => format!("{value:.0}°C"),
```

- [ ] **Step 4: 模板泛化**

把 `expand_cpu_temp_template` 整体替换为：

```rust
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
```

把 `display_value` 中的调用改为：

```rust
    if let Some(expanded) = expand_temp_template(template, &text, snapshot) {
        return expanded;
    }
```

在 `crates/render/src/lib.rs` 中：

把 `value_text` 的 match 里 `"cpu_temp"` 分支之后加入：

```rust
        "gpu" => snapshot
            .gpu
            .as_ref()
            .map(|gpu| format!("{:.0}%", gpu.util_pct))
            .unwrap_or_default(),
        "gpu_temp" => snapshot
            .gpu
            .as_ref()
            .and_then(|gpu| gpu.temp_celsius)
            .map(|celsius| format!("{celsius:.0}°C"))
            .unwrap_or_default(),
```

把 `item_value_text` 改为：

```rust
/// 显示项参与布局与缓存键的值文本；`format` 含 `{cpu_temp}`/`{gpu_temp}` 时把温度内联进来。
/// 缓存键同样走这里，保证独立温度项被隐藏/删除后，温度变化仍会触发重绘。
fn item_value_text(item: &DisplayItem, snapshot: &Snapshot) -> String {
    let value = value_text(&item.id, snapshot);
    value::expand_temp_template(&item.format, &value, snapshot).unwrap_or(value)
}
```

- [ ] **Step 5: 默认显示项追加 `gpu`**

在 `crates/core/src/config.rs` 的 `default_items()` 中，`cpu_temp` 项之后加入：

```rust
        DisplayItem {
            id: "gpu".into(),
            label: "GPU".into(),
            unit: "%".into(),
            format: "{value} {gpu_temp}".into(),
            color: "#B197FC".into(),
            visible: true,
        },
```

- [ ] **Step 6: 运行测试确认通过**

Run: `cargo test -p trafficview-render -p trafficview-core`

Expected: 全部通过。

- [ ] **Step 7: 提交**

```bash
git add crates/render/src/value.rs crates/render/src/lib.rs crates/core/src/config.rs
git commit -m "feat(render): 新增 GPU 显示项与 gpu_temp 内联模板"
```

---

### Task 6: 初始窗口尺寸与 README

**Files:**
- Modify: `crates/ui-x11/src/lib.rs`
- Modify: `README.md`

- [ ] **Step 1: 更新失败测试**

把 `initial_sizing_accounts_for_optional_items` 中的断言 `assert_eq!(sizing.items.len(), 5);` 改为：

```rust
        assert_eq!(sizing.items.len(), 6);
```

Run: `cargo test -p trafficview-ui-x11 initial_sizing_accounts_for_optional_items`

Expected: FAIL（`sizing_snapshot()` 还没有 GPU，实际只有 5 项）。

- [ ] **Step 2: 估算快照补 GPU**

把 `sizing_snapshot()` 改为：

```rust
/// 估算初始窗口尺寸时假定可选显示项已有读数，避免首帧高度跳变。
fn sizing_snapshot() -> Snapshot {
    let mut snapshot = empty_snapshot();
    snapshot.cpu_temp = Some(0.0);
    snapshot.gpu = Some(trafficview_core::GpuSample {
        name: "GPU".into(),
        util_pct: 0.0,
        mem_used: 0,
        mem_total: 0,
        freq_mhz: None,
        temp_celsius: Some(0.0),
        stale: false,
        observed_at_ms: None,
    });
    snapshot
}
```

Run: `cargo test -p trafficview-ui-x11`

Expected: 全部通过。

- [ ] **Step 3: README 说明**

在 `README.md` 的「主要字段」显示项说明处补充：

```markdown
- `gpu`：主 GPU 利用率，默认 `format = "{value} {gpu_temp}"`，温度内联显示；
  检测不到 GPU 时整行隐藏。已有配置需手动追加：

  ```toml
  [[overlay.items]]
  id = "gpu"
  label = "GPU"
  unit = "%"
  format = "{value} {gpu_temp}"
  color = "#B197FC"
  visible = true
  ```

- 显示模板支持 `{cpu_temp}` 与 `{gpu_temp}` 占位符；无对应读数时温度段为空并压缩空白。
```

- [ ] **Step 4: 提交**

```bash
git add crates/ui-x11/src/lib.rs README.md
git commit -m "feat(ui-x11): 初始尺寸估算覆盖 GPU 项并更新文档"
```

---

### Task 7: 全量验证

**Files:**
- 无新增；必要时修复前序任务遗漏。

- [ ] **Step 1: 格式与静态检查**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`

Expected: 无输出、退出码 0。

- [ ] **Step 2: 全量测试**

Run: `dbus-run-session -- cargo test --workspace`

Expected: 全部通过。

- [ ] **Step 3: 本机实机验证（主代理执行，不派子代理）**

```bash
cargo build --release -p trafficviewd -p trafficview
```

在隔离 XDG 配置与私有会话总线下启动 daemon 与悬浮窗，确认：

- `trafficview status --format json` 的 `gpu` 含 `util_pct` 与 `temp_celsius`（本机为 AMDGPU）。
- 默认配置下悬浮窗第三行显示 `GPU xx%  yy°C`，窗口尺寸稳定无跳变。
- 隐藏 `gpu` 项后窗口回到两行。

- [ ] **Step 4: 提交剩余改动**

```bash
git status --short
# 仅当有遗漏文件时：
git add -A
git commit -m "test: GPU 温度显示全量验证"
```
