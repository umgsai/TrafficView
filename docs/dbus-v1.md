# TrafficView D-Bus v1 契约（M1 冻结）

本文件是 M1 冻结的 D-Bus v1 契约。后续阶段只允许新增可选键或新接口，不得重命名、
改变类型或改变现有字段的单位/语义。

## 名称

```text
BUS:   session bus
NAME:  com.trafficview.Daemon1
PATH:  /com/trafficview/Daemon1
IFACE: com.trafficview.Daemon1
```

daemon 注册失败时只会退化为“已有实例”，不会并行启动第二个采集器。

## Properties

| 名称 | 类型 | 语义 |
|---|---|---|
| `SampleInterval` | `u` | 采样周期毫秒，允许 500–30000；默认 1000 |
| `Interfaces` | `as` | `/sys/class/net` 下的网卡名，排序稳定 |
| `ActiveInterface` | `s` | 监控网卡；空字符串表示自动（默认路由网卡） |

属性通过标准 `org.freedesktop.DBus.Properties.Get` 读取。变更通过自定义
`ConfigChanged` 信号通知；`Interfaces` 使用 `InterfacesChanged(as)`。

## Methods

| 名称 | 签名 | 语义 |
|---|---|---|
| `GetSnapshot` | `() -> a{sv}` | 返回最近一次完整快照；无快照或暂停时返回 `org.freedesktop.DBus.Error.Failed` |
| `SetActiveInterface` | `s -> ()` | 未知网卡返回 `Failed` 且保持原值；空字符串表示自动 |
| `SetInterval` | `u -> ()` | 超出 500–30000 返回 `InvalidArgs`；成功后持久化并广播 `ConfigChanged` |
| `SetOverlayVisible` | `b -> ()` | 更新悬浮窗显隐，持久化并广播 `ConfigChanged` |
| `ReloadConfig` | `() -> ()` | 重新读取 TOML；网卡不存在时返回 `Failed` 且不应用 |
| `Pause` | `b -> ()` | 暂停/恢复采样；暂停期间 `GetSnapshot` 返回 `Failed("daemon is paused")` |
| `Quit` | `() -> ()` | 请求 daemon 退出 |

暂停恢复后重建采集基线，暂停时长不会被计入速率。

## Signals

| 名称 | 签名 | 语义 |
|---|---|---|
| `Sample` | `a{sv}` | 每个采样周期广播一次完整快照 |
| `Alert` | `sss` | `level, code, message`（M1 保留，暂未产生） |
| `InterfacesChanged` | `as` | 启动时及网卡集合变化时广播 |
| `ConfigChanged` | `` | 间隔、网卡、悬浮窗显隐或暂停状态变化 |

## 快照字段

顶层必需键：`ts_ms`、`net`、`cpu`、`mem`。`disk`、`gpu` 是可选键，M1 未采集时
直接省略（D-Bus variant 没有 null 语义）；`temps` 为空 `av`，`plugin` 为空 `a{ss}`。

```text
ts_ms  t      CLOCK_MONOTONIC 毫秒；保持单调，不与墙上时钟耦合
net    a{sv}  iface s, rx_bps t, tx_bps t, rx_total t, tx_total t
cpu    a{sv}  total_pct d, per_core ad, [freq_mhz u], [load ad]
mem    a{sv}  used_pct d, total t, available t, swap_pct d
disk   a{sv}  [可选] read_bps t, write_bps t
gpu    a{sv}  [可选] name s, util_pct d, mem_used t, mem_total t
temps  av     [{label s, celsius d}]
plugin a{ss}  插件指标（M1 为空）
```

- 速率单位为字节/秒；`*_total` 为自网卡启动以来的累计字节数。
- `cpu.total_pct`、`mem.used_pct`、`swap_pct`、`gpu.util_pct` 为 0–100 的百分比。
- 未采集的可选结构必须省略，不能用 `0` 假装已采集。
- CLI `--json` 还原快照时将省略的 `disk`/`gpu` 表示为 `null`。

## 错误与降级

- 无快照：`GetSnapshot` 返回 `Failed("no sample available yet")`。
- 暂停：`GetSnapshot` 返回 `Failed("daemon is paused")`，`trafficview status` 退出码为 2。
- 未知网卡：`SetActiveInterface` 返回 `Failed`，原网卡保持不变。
- 非法周期：`SetInterval` 返回 `InvalidArgs`。
- 无默认路由：daemon 记录警告并保持存活，不产生伪造读数。

## 契约测试

```bash
dbus-run-session -- cargo test -p trafficview-daemon --test dbus
```

覆盖：必需键与嵌套签名、可选键省略、`temps`/`plugin` 空集合、属性读取、
`SetInterval` 边界、`Sample` 的 `ts_ms` 单调、未知网卡拒绝、`SetOverlayVisible`
触发 `ConfigChanged`。

## M2 增量（非破坏）

M2 没有新增 D-Bus 成员，也没有修改任何既有字段的名称、类型、单位或语义。

- GNOME Shell 扩展、Plasma widget 与 waybar 输出都只消费 `GetSnapshot` 与 `Sample`。
- `trafficview status --format waybar --allow-disconnected` 在 daemon 不可达时输出
  `class="disconnected"` 的单行 JSON；这是 CLI 行为，不改变 D-Bus 错误语义。
- 相反，daemon 暂停时 `GetSnapshot` 仍返回 `Failed("daemon is paused")`。

