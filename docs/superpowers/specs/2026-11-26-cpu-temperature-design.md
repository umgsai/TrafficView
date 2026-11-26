# CPU 温度监控（悬浮窗显示项）设计

日期：2026-11-26
状态：已确认，待实现

## 背景

`hwmon` 温度采集已存在：[hwmon.rs](file:///home/Shang/workspace/TrafficView/crates/collectors/src/hwmon.rs) 扫描
`/sys/class/hwmon/hwmon*/temp*_input`，结果写入 `Snapshot.temps`，托盘 tooltip 与
`status --format json` 已能看到全部传感器。缺口有三处：

- 没有“哪个传感器代表 CPU”的语义，`temps` 里混有 NVMe、GPU、主板与无线网卡。
- 悬浮窗渲染只认识 `net_up` / `net_down` / `cpu` / `mem` 四个 id。
- 只扫 hwmon，只有 `/sys/class/thermal` 的设备（常见于 ARM）拿不到温度。

## 目标

1. 悬浮窗可显示一个 CPU 温度显示项，无可用传感器时整行隐藏。
2. 默认按固定优先级自动识别 CPU 传感器，并允许配置覆盖。
3. 采集层补充 `/sys/class/thermal` 来源，与 hwmon 合并去重。

## 非目标

- CLI 文本、waybar/polybar/tmux 的专属温度输出。
- 温度告警、历史记录、设置界面编辑传感器。
- 修复 `acpitz` 等多路同 label 传感器在 tooltip 里的重复显示（既有行为，不在本次范围）。

## 数据流

```
/sys/class/hwmon ─┐
                  ├─> collectors 合并去重 ─> Snapshot.temps
/sys/class/thermal┘                              │
                                                 │ daemon 按配置解析
                                                 ▼
                                   Snapshot.cpu_temp: Option<f32>
                                                 │
                                     D-Bus / JSON │
                                                 ▼
                         render 的 cpu_temp 项（None 时跳过）
```

解析放在 daemon 而不是渲染层：Overlay、Wayland、CLI、后续告警与历史都消费同一个
`Snapshot`，避免每处重复实现“选哪个传感器”。代价是 D-Bus 契约新增一个可选键。

## 采集层

新增 `crates/collectors/src/thermal.rs`：

- `read_thermal_temps(root: &Path) -> Vec<TempSample>` 扫描
  `thermal_zone*/type` 与 `thermal_zone*/temp`，label 为 `thermal/<type>`，温度按
  `temp / 1000` 得到摄氏度；不可读、非数字、非有限值以及 `-50..=150` 之外的值丢弃。
- `merge_thermal_temps(hwmon: Vec<TempSample>, zones: Vec<TempSample>) -> Vec<TempSample>`：
  先放入全部 hwmon 读数；对每个 zone，若其 `type` 与任一 hwmon 的 chip 名（label 第一段，
  大小写不敏感）相同，则丢弃该 zone，否则追加；最后按 label 稳定排序。
- 去重示例：本机 hwmon 已有 `acpitz` 与 `iwlwifi_1`，对应 thermal zone 全部丢弃；
  `k10temp/Tctl` 保留。没有任何 hwmon 的 ARM 设备则保留 `thermal/cpu_thermal`。

`crates/daemon/src/service.rs` 改为：

```rust
let hwmon = read_temps(&sys_class.join("hwmon"));
let zones = read_thermal_temps(&sys_class.join("thermal"));
let temps = merge_thermal_temps(hwmon, zones);
```

`CollectorStatus.temps` 在合并结果为空时返回 `available=false`，原因改为
`no hwmon or thermal sensors`。

## 模型与解析

`Snapshot` 新增字段：

```rust
#[serde(default)]
pub cpu_temp: Option<f32>,
```

`Snapshot::new` 置为 `None`；字段缺失的旧 JSON 反序列化为 `None`。

新增 `crates/core/src/temp.rs`：

```rust
pub fn cpu_temperature(temps: &[TempSample], sensor: Option<&str>) -> Option<f32>
```

规则按顺序执行，label 与 chip 比较均为大小写不敏感：

1. `sensor` 非空时，先找完整 label 精确相等者，再找 label 包含该子串者；命中即返回。
2. `coretemp` 且 label 含 `package`。
3. `k10temp` 的 `Tdie`，其次 `Tctl`。
4. `zenpower` 的 `Tdie`，其次 `Tctl`。
5. `x86_pkg_temp`。
6. `cpu_thermal`、`cpu-thermal`、`soc_thermal`、`soc-thermal`。
7. 无命中返回 `None`。

覆盖值未命中时回退 2–6 的自动规则，不报错。任何情况下都不拿 NVMe、GPU、主板温度兜底。

daemon 在合并温度后执行：

```rust
snapshot.cpu_temp = cpu_temperature(&snapshot.temps, Some(&config.overlay.cpu_temp_sensor));
```

空字符串等价于 `None`。

## 配置

`OverlayConfig` 新增：

```rust
#[serde(default)]
pub cpu_temp_sensor: String,
```

默认空字符串。`config set overlay.cpu_temp_sensor <label>` 写入该字段。

`default_items()` 追加：

```toml
[[overlay.items]]
id = "cpu_temp"
label = "TEMP"
unit = "°C"
format = "{value}"
color = "#FFA94D"
visible = true
```

已有用户配置不会被改写，需要手动追加该数组项。

## 渲染

`crates/render/src/value.rs`：

- `is_known_id` 接受 `cpu_temp`，皮肤校验据此放行。
- `numeric_value("cpu_temp")` 返回 `snapshot.cpu_temp as f64`。
- `display_value` 对 `cpu_temp` 输出 `format!("{value:.0}°C")`。
- `value_text` 同步输出整数摄氏度，用于内置布局的尺寸与绘制文本。

`crates/render/src/lib.rs`：

- 内置布局与皮肤渲染在遍历显示项时，遇到 `cpu_temp` 且 `snapshot.cpu_temp` 为 `None`
  则跳过该项；可见项数量减少后高度按现有公式收缩，不绘制 `--`。
- 其它 id 行为不变。

## D-Bus 与 JSON

- `snapshot_to_dbus` 在 `cpu_temp` 为 `Some` 时写入顶层 `cpu_temp`（`f64`），`None` 时省略。
- `snapshot_from_dbus` 读取可选键；缺失视为 `None`。
- JSON 由 `Snapshot` 的 serde 派生自动带出 `cpu_temp`，旧的快照文件缺该键时仍可加载。
- 更新 `docs/dbus-v1.md` 的 `Sample` 字段表。

## 错误处理

| 场景 | 行为 |
| --- | --- |
| 单个传感器文件损坏或数值越界 | 丢弃该条，其它温度照常 |
| hwmon 与 thermal 都为空 | `cpu_temp=None`，显示项隐藏，状态原因写明 |
| 配置的传感器不存在 | 回退自动规则；仍无命中则隐藏 |
| 采样首帧尚未读到温度 | `cpu_temp=None`，首帧后出现；窗口高度随之变化一次 |
| 旧配置 / 旧快照 JSON 缺字段 | serde 默认值，正常工作 |

## 测试

- `collectors::thermal`：zone 解析、毫摄氏度换算、非法与越界值过滤、与 hwmon 按 chip 去重、
  无 hwmon 时保留 zone。
- `core::temp`：Intel Package、AMD Tdie/Tctl、zenpower、x86_pkg_temp、ARM cpu_thermal、
  覆盖值精确/子串/未命中回退、仅 NVMe/GPU 时返回 `None`。
- `core::config`：不含新字段的旧 TOML 反序列化成功；`config set` 写入覆盖值。
- `daemon::dbus`：`cpu_temp` 的写入/省略与往返解析。
- `render`：有/无 `cpu_temp` 时的布局项数量与高度；`display_value` 文本为整数摄氏度；
  `layout` 与 `skin` 渲染都跳过缺失项。

## 风险

- `k10temp` 的 `Tctl` 在部分 AMD 平台带偏移，可能略高于真实温度；优先 `Tdie`，并允许用户覆盖。
- thermal zone 的 `type` 命名因平台而异，仅把常见的 cpu/soc 名称纳入自动规则。
- 新增可选 D-Bus 键对旧客户端无害；但 1.0.3 之前的 `skin preview` 快照 JSON 没有该键，
  预热预览时该项会隐藏，属于预期行为。
