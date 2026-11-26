# GPU 使用率与温度显示设计

日期：2026-11-26
状态：已确认，待实现

## 背景

悬浮窗已有上下行速率、CPU（含内联温度）、MEM 显示项。`Snapshot.gpu/gpus` 已提供
主 GPU 的 `util_pct`，但 `GpuSample` 没有温度字段：NVIDIA `nvidia-smi` 查询里
明明带了 `temperature.gpu` 却被丢弃，AMD 的温度只能从 hwmon 列表里看到
（`amdgpu/edge`），没有与 GPU 关联。

目标：在 CPU 下面一行展示 GPU 的使用率和温度，风格与 CPU 行一致，温度内联，
不单独占显示项，无 GPU 时整行隐藏。

## 目标

1. 悬浮窗新增一行 `GPU 12%  55°C`（GPU 名称 + 使用率 + 内联温度，右侧留空）。
2. 温度归属于 `GpuSample`，多 GPU 时每块卡都能带自己的温度。
3. 无 GPU 时整行隐藏；有 GPU 无温度时只显示使用率。

## 非目标

- 多 GPU 同时显示或配置选择 GPU（固定使用 `Snapshot.gpu`，即 `gpus[0]`）。
- 显存占用、GPU 频率等额外指标。
- GPU 温度告警、历史、设置界面编辑。
- Intel 后端的温度采集（暂为 `None`）。

## 最终效果

```
↑ 2.2 KB/s      ↓ 900 B/s
CPU 7%  76°C    MEM 37%
GPU 12%  55°C
```

无温度读数时第三行为 `GPU 12%`；无 GPU 时第三行整体消失，窗口高度收缩。

## 数据流

```
AMD:  card*/device/hwmon/hwmon*/temp*_{label,input} ─┐
NVIDIA: nvidia-smi temperature.gpu / NVML GetTemperature ─┤
                                                          ├─> GpuSample.temp_celsius
Intel: 无（None）─────────────────────────────────────────┘        │
                                                                   ▼
                                            Snapshot.gpu / gpus（D-Bus、JSON 自动携带）
                                                                   │
                                                                   ▼
                              render 的 gpu 项（format = "{value} {gpu_temp}"）
```

## 模型与采集

`crates/core/src/lib.rs` 的 `GpuSample` 新增：

```rust
/// GPU 核心温度（摄氏度）；后端拿不到时为 `None`。
#[serde(default)]
pub temp_celsius: Option<f32>,
```

各后端填充规则：

- **AMD**：`amd_card()` 在 `card*/device/hwmon/hwmon*/` 下按 `temp*_label` 选择
  `edge`，其次 `junction`、`mem`；读数为毫摄氏度，除以 1000 得摄氏度。
  不可读、非数字、非有限值以及 `-50..=150` 之外的值丢弃。该路径与 DRM 卡一一对应，
  多卡不会错配。找不到任何合法读数时 `temp_celsius = None`，不影响利用率。
- **NVIDIA nvidia-smi**：`parse_nvidia_smi` 解析第 5 列 `temperature.gpu`，
  空值/`[N/A]`/非数字时为 `None`。
- **NVIDIA NVML**：动态加载增加 `nvmlDeviceGetTemperature`（`NVML_TEMPERATURE_GPU = 0`），
  符号缺失或调用失败时为 `None`。
- **Intel**：`temp_celsius = None`。

## D-Bus 与 JSON

- `gpu` 与 `gpus` 元素字典新增可选键 `temp_celsius`（`d`），`None` 时省略；
  反序列化缺失键为 `None`。
- JSON 由 serde 派生自动带出，旧快照缺字段反序列化为 `None`。
- `docs/dbus-v1.md` 的 GPU 字段说明同步更新。

## 渲染

`crates/render/src/value.rs`：

- `BUILTIN_IDS` 扩为 7 项，新增 `gpu`、`gpu_temp`。
- `numeric_value`：`gpu` 取 `snapshot.gpu.util_pct`，`gpu_temp` 取
  `snapshot.gpu.temp_celsius`；无对应数据时返回 `None`。
- `is_available`：`gpu` 与 `gpu_temp` 在 `snapshot.gpu` 为 `None` 时不可用；
  `gpu_temp` 还要求 `temp_celsius` 有值。不可用时内置布局与皮肤都跳过该项。
- `display_value`：`gpu` 输出 `{value:.1}%`，`gpu_temp` 输出 `{value:.0}°C`。
- `expand_cpu_temp_template` 泛化为同时支持 `{cpu_temp}` 与 `{gpu_temp}` 的模板展开；
  空白规则不变：有读数时原样保留模板空白，无读数时压缩空白避免多余间距。

`crates/render/src/lib.rs`：

- `value_text` 增加 `gpu`（`{:.0}%`）与 `gpu_temp`（整数 °C，无值空串）分支。
- 布局、缓存键继续共用 `item_value_text`，温度变化会触发重绘。

## 默认配置

`default_items()` 只追加一项（不默认添加独立 `gpu_temp` 项）：

```toml
[[overlay.items]]
id = "gpu"
label = "GPU"
unit = "%"
format = "{value} {gpu_temp}"
color = "#B197FC"
visible = true
```

已有配置文件不会被自动改写，需要手动追加上述片段；README 给出示例。
不新增任何配置键。

## 初始窗口尺寸

`crates/ui-x11/src/lib.rs` 的 `sizing_snapshot()` 目前只假定 `cpu_temp` 有读数。
新增 `gpu` 默认项后，需要同样为 `gpu` 填一个占位（利用率 0、温度 0），
使初始窗口按 3 行估算，避免首帧高度跳变；无 GPU 的机器首帧收缩一次，属预期。

## 错误处理

| 场景 | 行为 |
| --- | --- |
| 无 GPU / GPU 后端不可用 | `snapshot.gpu = None`，`gpu` 项整行隐藏 |
| 有 GPU 但无温度 | 只显示 `GPU 12%`，不显示温度占位符 |
| 单个温度文件损坏或数值越界 | 仅 `temp_celsius = None`，利用率照常 |
| 旧配置 / 旧快照 JSON 缺字段 | serde 默认值，正常工作 |
| 旧 D-Bus 客户端 | 新增可选键，无影响 |

## 测试

- `collectors::gpu`：AMD hwmon 目录的 edge 优先与 junction/mem 回退、毫摄氏度换算、
  非法与越界值过滤、无 hwmon 目录时为 `None`；nvidia-smi 第 5 列解析与非法值处理。
- `core`：`GpuSample` 新字段的 serde 缺省。
- `daemon::dbus`：`temp_celsius` 的写入/省略与往返解析。
- `render`：`gpu` 项有/无 GPU 时的布局项数量与高度；`{gpu_temp}` 内联文本；
  无温度时只显示百分比；皮肤路径跳过不可用项。
- `ui-x11`：初始尺寸估算覆盖 `gpu` 项。

## 风险

- AMD `edge`/`junction`/`mem` 的可用性因卡而异，已按优先级回退；极端旧驱动可能
  没有 hwmon 目录，此时只显示利用率。
- NVML 动态符号在部分驱动版本缺失，已按失败即 `None` 处理。
- 多 GPU 固定显示 `gpus[0]`，与托盘 tooltip 口径一致；后续如需选择 GPU 再引入配置。
