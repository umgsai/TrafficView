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

顶层必需键：`ts_ms`、`net`、`cpu`、`mem`。`cpu_temp`、`disk`、`gpu`、`filesystems`、
`battery`、`uptime_s` 是可选键，未采集时直接省略（D-Bus variant 没有 null 语义）；
`temps` 为空 `av`，`plugin` 为空 `a{ss}`。

```text
ts_ms       t      CLOCK_MONOTONIC 毫秒；保持单调，不与墙上时钟耦合
net         a{sv}  iface s, rx_bps t, tx_bps t, rx_total t, tx_total t
cpu         a{sv}  total_pct d, per_core ad, [freq_mhz u], [load ad]
mem         a{sv}  used_pct d, total t, available t, swap_pct d
disk        a{sv}  [可选] read_bps t, write_bps t
gpu         a{sv}  [可选] 主 GPU 兼容别名：name s, util_pct d, mem_used t,
                   mem_total t, [freq_mhz u], stale b, [observed_at_ms t]
gpus        av     [可选] 全部 GPU，元素同 `gpu`
cpu_temp    d      [可选] 代表 CPU 的温度（摄氏度），无可用传感器时省略
temps       av     [{label s, celsius d}]
filesystems av     [可选] [{mount s, total t, available t, used_pct d}]
battery     a{sv}  [可选] percent d, status s
uptime_s    t      [可选] `CLOCK_BOOTTIME` 秒
plugin      a{ss}  插件指标（M1 为空）
```

- 速率单位为字节/秒；`*_total` 为自网卡启动以来的累计字节数。
- `cpu.total_pct`、`mem.used_pct`、`swap_pct`、`gpu.util_pct` 为 0–100 的百分比。
- `gpu` / `gpus[]` 可选键 `temp_celsius`（`d`）：GPU 核心温度（摄氏度），无读数时省略。
- 未采集的可选结构必须省略，不能用 `0` 假装已采集。
- CLI `--json` 还原快照时将省略的可选结构表示为 `null`。

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

## M3 增量（非破坏）

M3 只新增成员，未修改任何既有字段/信号语义。

### 新增 Properties

| 名称 | 类型 | 语义 |
|---|---|---|
| `ColorScheme` | `s` | `light` 或 `dark`；由 portal 与用户 `theme` 配置决定，变化时触发 `ConfigChanged` |

### 新增 Methods

| 名称 | 签名 | 语义 |
|---|---|---|
| `GetTrafficSummary` | `ss -> a{sv}` | `(scope, iface_stable)`；scope 只接受 `day`/`month`，否则 `InvalidArgs`；空 stable 表示所有网卡求和 |
| `GetInterfaceDetails` | `() -> a{sv}` | 键为接口名，值为 `a{sv}`：`stable s, mac s, ifindex u, operstate s, carrier b, loopback b, active b` |

`GetTrafficSummary` 返回键固定为：

```text
scope        s    day | month
start        s    day: YYYY-MM-DD；month: 当月首日
end          s    day: YYYY-MM-DD；month: 当月末日
rx           t    原始字节增量总和
tx           t    原始字节增量总和
interfaces   av   [{stable s, name s, rx t, tx t}]
```

历史库不可用时返回 `Failed("traffic history is unavailable")`，不得伪造 0 值。
原 `Interfaces` 属性保持 `as` 兼容，新增详情只走 `GetInterfaceDetails`。

### Alert 信号语义补充

`Alert(sss)` 的签名不变。M3 开始实际产生事件：越限事件 `level` 取规则配置
（`info`/`warning`/`critical`），恢复事件固定为 `info`，`code` 与越限事件一致，
消息以“已恢复”开头。前端可据此清除告警色。

### 配置新增

```toml
theme = "dark"                 # auto | light | dark（默认 dark）
language = "auto"              # auto | zh | en（默认 auto，跟随 LC_ALL/LANG）
network_smoothing = "none"     # none | moving_average_3 | ema
interface_whitelist = []       # 优先于默认路由
[history]
retention_days = 90
timezone = ""                  # 空=系统本地；或 UTC / +08:00
[alerts]
rules = []                     # 默认不启用，避免升级后突然打扰
```

每条告警规则：`enabled/metric/op/threshold/duration_s/cooldown_s/quiet_hours/level`；
`metric` 取值 `cpu_pct|mem_pct|net_rx_bps|net_tx_bps|month_bytes`。

## M4 增量（非破坏）

M4 只新增可选快照键与一个只读属性，未修改任何既有字段/信号语义。

### 新增快照键

`filesystems`（`av`）、`battery`（`a{sv}`）、`uptime_s`（`t`）、`gpus`（`av`），
语义见上文；`cpu.freq_mhz`、`cpu.load`、`disk` 与 `temps` 在本阶段开始实际填充。
可选 `processes`（`av`）表示进程级流量，仅在显式启用后出现，见下节。

### 进程级流量（M4，可选）

`process_traffic.backend` 取值 `off`（默认）`| ebpf | af_packet | auto`。
user daemon 不直接抓包，只读取 system bus helper 的聚合结果：

```text
processes  av  [{pid u, name s, rx_bps t, tx_bps t, rx_total t, tx_total t}]
```

默认 off 或 helper 不可用时省略该键；后端可用但当前无匹配进程时才发送空数组。
`auto` 优先 eBPF，能力不足再尝试 AF_PACKET。helper 接口：

```text
NAME: com.trafficview.Ebpf1
PATH: /com/trafficview/Ebpf1
Methods: GetProcesses() -> a{sv}, GetBackendStatus() -> a{sv}
Signals: 无
```

`GetBackendStatus` 返回 `available b, backend s, reason s, capabilities as`
（capabilities 为 `ebpf`/`af_packet`）。`GetProcesses` 返回 `processes` 数组；
helper 按调用者 UID 过滤，user daemon 只保留 Top 10。helper 缺失时
`CollectorStatus.processes` 为 `available=false`，原因包含
`trafficviewd-ebpf.service` 安装提示。单次查询上限 1 秒，失败不保留旧排行。
`UpdateConfig` 修改 `backend` 或 `sample_interval_ms` 后 daemon 立即重启消费者，
无需重启进程；查询周期取 `sample_interval_ms`，下限 1 秒。

本版本提供 `/usr/bin/trafficviewd-ebpf --backend af-packet`：它扫描
`/proc/net/{tcp,udp,tcp6,udp6}` 与 `/proc/<pid>/fd` 建立端口→inode→PID 映射，
按 2 秒周期刷新并只把能归属的帧计入对应 PID；无法归属的字节不分配给任何进程。
`GetProcesses` 每次调用都会刷新聚合，并通过 `GetConnectionUnixUser` 获取调用者 UID：
普通用户只能看到自己的进程，root 可取全部；身份无法确认时返回空数组。
helper 需要 `CAP_NET_RAW`，对应单元为 `trafficviewd-afpacket.service`，
system bus 策略为 `packaging/dbus/com.trafficview.Ebpf1.conf`。
`--backend ebpf` 在当前构建中固定返回 `available=false`；eBPF 内核程序、
`trafficviewd-ebpf.service` 的实机加载与真实抓包尚未验收。

```toml
[process_traffic]
backend = "off"   # off | ebpf | af_packet | auto
```

GPU 后端优先 NVML，失败回退 `nvidia-smi`；AMD 用 `gpu_busy_percent`；Intel 用
`fdinfo` 引擎差分，中间帧 `stale=true` 且保留原 `observed_at_ms`，超过 15 秒未更新
才省略。`gpu` 始终是主 GPU 别名，多 GPU 详情只通过 `gpus` 提供。

### 新增 Properties

| 名称 | 类型 | 语义 |
|---|---|---|
| `CollectorStatus` | `a{sv}` | 键为采集器名（`disk/temps/battery/filesystems/uptime/cpufreq/loadavg`），值为 `a{sv}`：`available b, reason s`；区分“零值”与“不可用” |

### 配置读写（M4）

| 名称 | 签名 | 语义 |
|---|---|---|
| `GetConfig` | `() -> a{sv}` | 完整配置；键与 `UpdateConfig` 对称 |
| `UpdateConfig` | `a{sv} -> ()` | 按顶层键部分更新；在副本上合并并完整校验，原子保存成功后才提交 |

顶层键：`sample_interval_ms u`、`active_interface s`、`memory_include_cache b`、
`interface_blacklist as`、`interface_whitelist as`、`network_smoothing s`
（`none|moving_average_3|ema`）、`theme s`（`auto|light|dark`）、`history a{sv}`
（`retention_days u, timezone s`）、`alerts a{sv}`（`rules av`，每项
`enabled b, metric s, op s, threshold d, duration_s u, cooldown_s u,
quiet_hours s, level s`）、`overlay a{sv}`（`visible b, position ai, opacity d,
click_through b, always_on_top b, skin s, units s, items av`，每项
`id/label/unit/format/color s, visible b`）、`plugins a{sv}`（`enabled b`）、
`process_traffic a{sv}`（`backend s`：`off|ebpf|af_packet|auto`）。

顶层缺失键保持不变；嵌套小节按键合并；`rules`/`items` 数组整体替换。
未知键、类型错误、`Config::validate` 失败或配置的网卡不存在均返回 `InvalidArgs`，
且不修改内存与磁盘。成功更新后广播 `ConfigChanged`。设置界面不得直接写配置文件。

### `plugin` 快照键与插件宿主（M4 Task 3）

`plugin` 保持 `a{ss}`，键为 `<plugin-id>.<item-id>`，值为插件上报的原始字符串。
插件缺失、崩溃或禁用时对应键省略，前端按既有规则显示 `--`，不伪造 0。

插件清单位于 `$XDG_DATA_HOME/trafficview/plugins/<id>/plugin.toml`（未设置时
`$HOME/.local/share/trafficview/plugins`）：

```toml
id = "example"
command = "/absolute/path/to/plugin"
args = []
interval_ms = 1000
capabilities = ["metrics"]
```

`command` 直接 exec（不经过 shell），必须是绝对路径；`id` 必须与目录名一致，
仅允许 `[A-Za-z0-9_-]`；`interval_ms` 不得小于 500。协议为单行 JSON：
request `{"t":<unix 秒浮点>,"interval_ms":<u32>}`，response
`{"items":[{id,name,unit,format}],"values":{id:value}}`；单帧上限 1 MiB，
非 UTF-8、未知字段、重复/非法 id 与值内换行均拒绝。

单次等待 2 秒，超时 kill；失败按 1/2/4/8/16/32/60 秒退避，连续 3 次失败后禁用，
只有 `ReloadConfig` 才重新启用。`CollectorStatus` 新增 `plugin.<id>` 键，
值为 `available b, reason s`；插件失败不影响其他键与基础指标。配置新增
`[plugins] enabled = true`，只控制是否运行插件，不清空已发布的 `plugin` 键语义。


