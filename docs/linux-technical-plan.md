# TrafficView — Linux 版 TrafficMonitor 技术实现方案

> 目标：在 Linux（X11 + Wayland）上实现一款「网速 / CPU / 内存 / 硬件指标」常驻监控工具，
> 覆盖 TrafficMonitor 的核心能力：悬浮窗、任务栏/顶栏常驻显示、皮肤、历史流量统计、插件扩展。
>
> 调研对象：[TrafficMonitor v1.86](https://github.com/zhongyang219/TrafficMonitor)（C++ 92.5% / C 7.5%，MFC，Anti-996 License）

---

## 0. 结论摘要（TL;DR）

1. **不要移植 TrafficMonitor 的代码结构**。它强依赖 Win32：`GetIfTable2` 读网卡、`PDH/GetSystemTimes` 读 CPU、`GlobalMemoryStatusEx` 读内存、`SetParent(Shell_TrayWnd)` 嵌入任务栏、`UpdateLayeredWindow` 做 PNG 透明皮肤。Linux 只有「逐项重写采集」这一条路，但**产品模型（DisplayItem + 皮肤布局表 + 插件指标）可以完整复用**。
2. **Linux 没有统一的"任务栏 API"**，且 Wayland 禁止客户端绝对定位（GNOME/Mutter 明确拒绝 `wlr-layer-shell`）。因此「任务栏显示」必须拆成三条独立路径：**托盘图标（SNI）＋ 悬浮窗（X11 override-redirect / Wayland layer-shell）＋ 桌面扩展（GNOME Shell / Plasma widget）**。
3. **架构上做进程拆分**：`trafficviewd` 常驻守护进程统一采集，通过 D-Bus 广播采样；托盘、悬浮窗、Shell 扩展、waybar 模块、CLI 都只是消费者。这是 Linux 相比 Windows 单进程 MFC 的结构性优势，也避免了多个前端各自读 `/proc` 导致读数不一致。
4. **技术栈推荐 Rust + 纯自绘（tiny-skia / cosmic-text）+ 纯 Rust 系统集成（zbus / ksni / x11rb / wayland-client）**，产物是 ~10MB 静态二进制，无 GTK/Qt/运行时依赖，符合"零依赖、开箱即用"的偏好。
5. **最小可行产品（MVP）范围**：网速 + CPU + 内存 + 托盘图标 + X11 悬浮窗 + TOML 配置。硬件温度/GPU/进程级流量/插件放在 M3-M4 阶段性交付。

---

## 1. TrafficMonitor 拆解

### 1.1 功能矩阵（v1.86）

| 类别 | 能力 | 说明 |
|---|---|---|
| 网络 | 实时上/下行速率 | 自动或手动选择网卡；无流量网卡自动跳过 |
| 网络 | 网络详细信息 | 连接列表、IP、网卡信息 |
| 系统 | CPU 利用率、内存利用率、CPU 频率 | 主窗口与任务栏窗口可分别勾选显示项 |
| 硬件 | 硬盘读写、GPU 利用率、温度 | 通过 LibreHardwareMonitor（标准版需管理员权限；1.86 起仅 Lite 版） |
| 展示 | 主悬浮窗（可换肤） | PNG 透明背景、每项独立颜色/字体 |
| 展示 | 嵌入任务栏窗口 | 多显示器、Win10/11 深浅色自适应 |
| 展示 | 通知区图标 | 图标内动态绘制速率数字 |
| 交互 | 右键菜单、选项设置、鼠标穿透、置顶、透明度 | |
| 数据 | 历史流量统计（日/月） | 本地文件累计 |
| 扩展 | 插件系统（1.82+） | `plugins/*.dll`，`CreateInstance()` 导出 |
| 系统 | 开机自启、阈值通知、自动更新 | |

### 1.2 Windows 侧实现要点（源码路径推断）

| 关注点 | Windows 实现 | 备注 |
|---|---|---|
| 网卡速率 | `GetIfTable2()` → `MIB_IF_ROW2.InOctets/OutOctets`，按采样周期差分 | 64 位计数器；WiFi/虚拟网卡需过滤 |
| CPU | `PDH` 性能计数器或 `GetSystemTimes` 差分 | 与任务管理器口径对齐有讲究 |
| 内存 | `GlobalMemoryStatusEx()` | 用 `ullAvailPhys` 计算占用率 |
| 透明皮肤 | Layered Window (`WS_EX_LAYERED`) + `UpdateLayeredWindow`，贴 `background.png/bmp` | 1.85 起支持 PNG 透明通道 |
| 皮肤配置 | `skin.ini`（1.80 起 `skin.xml`），定义每个显示项的 rect / 字体 / 颜色 / 对齐 | **这个模型值得 1:1 复用** |
| 任务栏嵌入 | `FindWindow("Shell_TrayWnd")` + `SetParent` 子窗口，监听 `TaskbarCreated` 处理 Explorer 重启 | 有多显示器 `Shell_SecondaryTrayWnd` 变体 |
| 托盘图标 | 运行时把数字画进 `HICON` | 每秒重绘 |
| 插件 | 纯虚类 `ITMPlugin`：`GetItemCount/GetItemName/GetItemId/GetItemValue`、`DataRequired()`、`OnInitialize(ITrafficMonitor*)` | 宿主提供回调取窗口句柄、绘制 PNG |
| 权限 | 标准版需管理员（读温度传感器） | Lite 版免提权 |

### 1.3 可以直接复用的设计（跨平台）

1. **DisplayItem 模型**：每个指标是一个可配置项（`id / 名称 / 单位 / 格式模板 / 颜色 / 字体 / 是否显示 / 顺序`）。主窗口和任务栏窗口各自持有一份显示项列表。
2. **皮肤 = 背景图 + 布局表**：`[[item]] rect=[x,y,w,h]`，与 `skin.ini` 几乎一一对应，可写转换器迁移现有皮肤。
3. **采集与展示分离**：逻辑上已经是「采集定时器 → 快照 → 渲染」，Linux 上把它变成进程边界即可。
4. **多网卡自动选择策略**、**单位换算（1024 进制）**、**阈值告警状态机**、**日月流量累计**。

### 1.4 必须重写的部分

| Windows 机制 | Linux 替代 | 难度 |
|---|---|---|
| `GetIfTable2` | `/proc/net/dev` 或 netlink `RTM_GETLINK`（`IFLA_STATS64`） | 低 |
| `GetSystemTimes` / PDH | `/proc/stat` 差分 | 低 |
| `GlobalMemoryStatusEx` | `/proc/meminfo` | 低 |
| LHM 温度/GPU | `hwmon` sysfs / `drm` sysfs / NVML | 中 |
| 任务栏嵌入 | SNI 托盘 + layer-shell 悬浮窗 + 桌面扩展 | **高（生态碎片）** |
| Layered Window 皮肤 | ARGB buffer + 平台合成 | 中 |
| DLL 插件 | 子进程 JSON 协议（推荐）/ C ABI `.so` / WASM | 中 |
| 注册表自启动 | XDG autostart `.desktop` | 低 |

---

## 2. Linux 平台约束分析

### 2.1 会话类型是设计的一等公民

```text
X11  → 可以：override-redirect 窗口 / 任意绝对定位 / 全局快捷键(XGrabKey) / 鼠标穿透
Wayland → 不可以：绝对定位、全局快捷键
          可以：layer-shell（仅 wlroots 系 + 部分合成器）
```

### 2.2 桌面环境支持矩阵（这是本方案最重要的约束）

| 桌面 / 合成器 | 会话 | 托盘图标 | 悬浮窗 | 「任务栏文字」最佳路径 |
|---|---|---|---|---|
| XFCE 4.18 | X11 | 原生 SNI | override-redirect，任意定位 | 托盘图标（+悬浮窗） |
| Cinnamon / MATE / Budgie | X11 | 原生 SNI | 任意定位 | 托盘图标 |
| KDE Plasma 5/6 | X11 | 原生 SNI | 任意定位 | Plasma 托盘 / plasmoid |
| KDE Plasma 6 | Wayland | 原生 SNI | **layer-shell 可用** | 自研 plasmoid |
| GNOME 45+ | Wayland | 需 AppIndicator 扩展（Ubuntu 预装） | **不能 layer-shell**，只能普通窗口 | **自研 GNOME Shell 扩展** |
| GNOME (Xorg) | X11 | 需扩展 | 普通窗口（GNOME 不用 override-redirect） | 自研扩展 |
| Sway / Hyprland / river / niri | Wayland | waybar 的 SNI 模块 | **layer-shell 可用** | waybar 自定义模块（调 CLI 输出 JSON） |
| i3 / bspwm | X11 | 需 `stalonetray` / `trayer` / waybar | 任意定位 | waybar 模块 |

> 关键事实：GNOME/Mutter 明确拒绝实现 `wlr-layer-shell`（Mutter issue #1141），官方立场是「这类客户端应该做成 GNOME Shell 扩展」。因此 **GNOME 下的"任务栏显示"只能靠自研扩展**，不能靠层壳协议硬扛。

### 2.3 其他平台差异

- **权限**：读 `/proc`、`/sys` 普通用户即可；`hwmon` 温度通常可读；GPU 需 `video` 组；**eBPF 需 `CAP_BPF`+`CAP_PERFMON`（或 root）**；AF_PACKET 抓包需 `CAP_NET_RAW`。
- **DPI**：Wayland 分数缩放需按 `fractional-scale-v1` / `buffer_scale` 渲染物理像素，否则文字发虚；X11 依赖 `Xft.dpi`。
- **休眠/唤醒**：`CLOCK_REALTIME` 会跳变，采样时间戳必须用 `CLOCK_MONOTONIC`；唤醒后首次差分要丢弃。
- **容器**：Flatpak/Snap 沙箱看不到完整 `/proc`，eBPF 基本不可用。

---

## 3. 总体架构

```
┌─────────────────────────────────────────────────────────────────┐
│  Frontends（可任选，全部无状态）                                  │
│  ┌────────────┐ ┌─────────────┐ ┌──────────────┐ ┌───────────┐  │
│  │ tray (SNI) │ │ overlay     │ │ GNOME 扩展   │ │ CLI/waybar│  │
│  │  ksni      │ │ x11/layer   │ │ GJS + D-Bus  │ │ --json    │  │
│  └─────┬──────┘ └──────┬──────┘ └──────┬───────┘ └─────┬─────┘  │
└────────┼───────────────┼───────────────┼───────────────┼────────┘
         └───────────────┴───────┬───────┴───────────────┘
                                 │  D-Bus (unix socket / session bus)
                    ┌────────────▼─────────────┐
                    │  trafficviewd（守护进程） │
                    │  · 采样调度 timerfd 1s    │
                    │  · 指标聚合 / 平滑        │
                    │  · 历史流量持久化 SQLite  │
                    │  · 告警状态机 / 通知      │
                    │  · 插件宿主               │
                    └────────────┬─────────────┘
                                 │  syscalls / 文件系统
        ┌──────────┬─────────────┼─────────────┬──────────────┐
        ▼          ▼             ▼             ▼              ▼
   /proc/net  /proc/stat   /proc/meminfo  /sys/class/hwmon  eBPF
   rtnetlink  /proc/loadavg /proc/diskstats /sys/class/drm  (可选)
```

### 3.1 D-Bus 接口设计

```
BUS:     session bus
NAME:    com.trafficview.Daemon1
PATH:    /com/trafficview/Daemon1
IFACE:   com.trafficview.Daemon1

Properties:
  SampleInterval   u       采样周期（ms，500~30000）
  Interfaces       as      可用网卡名列表
  ActiveInterface  s       当前监控网卡（空=自动）

Methods:
  GetSnapshot() -> a{sv}                     一次性快照（CLI/扩展用）
  SetActiveInterface(s name)
  SetInterval(u ms)
  ReloadConfig()

Signals:
  Sample(a{sv} snapshot)    每个采样周期广播
  Alert(sss level, code, message)
  InterfacesChanged(as)
```

`snapshot` 结构（serde → `a{sv}`）：

```rust
struct Snapshot {
    ts_ms: u64,                 // CLOCK_MONOTONIC 毫秒
    net:  NetSample,            // rx_bps, tx_bps, iface, rx_total, tx_total
    cpu:  CpuSample,            // total_pct, per_core[], freq_mhz, load[3]
    mem:  MemSample,            // used_pct, total, available, swap_pct
    disk: DiskSample,           // read_bps, write_bps
    gpu:  Option<GpuSample>,
    temps: Vec<TempSample>,     // label, celsius
    plugin: BTreeMap<String, String>,
}
```

> 为什么用 D-Bus 而不是自定义 socket：桌面生态原生支持（GNOME 扩展用 Gio、KDE 用 QML、waybar 用 `dbus` 模块都能直连），且天然带单实例、权限、生命周期管理。`zbus` 是纯 Rust 实现，不依赖 `libdbus`。

---

## 4. 采集层设计

### 4.1 网络速率（核心）

**数据源优先级**

1. netlink `RTM_GETLINK` → `IFLA_STATS64`（`rx_bytes`/`tx_bytes`）：一次 syscall 拿全部网卡，开销最低，64 位计数器无溢出。
2. `/proc/net/dev` 兜底（容器、权限受限场景）：字段 `iface: rx_bytes rx_packets ... tx_bytes ...`。

**算法**

```
rate = (bytes_now - bytes_prev) / dt        // dt 用 CLOCK_MONOTONIC
若 dt > 5s（休眠唤醒）或 delta < 0（计数器重置）→ 丢弃本次，重新建立基线
显示值 = rate（B/s），按 1024 进位格式化为 B/s → KB/s → MB/s
```

- 与 TrafficMonitor 保持一致的 1024 进制；可选 bit/s 模式。
- 为了视觉稳定，可选 3 点滑动平均或 EMA(α=0.3)；默认关闭（原版是瞬时值）。
- 数字用**等宽数字（tabular-nums）+ 固定宽度槽位**，避免刷新时布局抖动。

**网卡发现与选择**

- 枚举 `/sys/class/net/*`，读 `operstate`、`carrier`、`type`、地址。
- 自动模式规则：① 取默认路由网卡（`/proc/net/route` 或 netlink `RTM_GETROUTE`）；② 若有多张活跃卡，取当前速率最高者；③ 黑名单：`lo`、`docker*`、`veth*`、`br-*`、`virbr*`、`tun*`（可配置白名单覆盖，VPN 用户常需要）。
- 订阅 netlink `RTM_NEWLINK` / `RTM_DELLINK`，热插拔（USB 网卡、WiFi 切换）时刷新菜单并重新建基线。

### 4.2 CPU

```text
/proc/stat:  cpu  user nice system idle iowait irq softirq steal guest guest_nice
total = Σ(前 8 项)
busy  = total - idle - iowait
usage = Δbusy / Δtotal          // 每核同理，cpu0..cpuN
```

- 频率：`/sys/devices/system/cpu/cpu*/cpufreq/scaling_cur_freq`（kHz），取平均或最高核。
- 负载：`/proc/loadavg`。
- 温度：`/sys/class/hwmon/hwmon*/name` 匹配 `coretemp`/`k10temp`，读 `temp1_input`（毫摄氏度）。
- 采样 1s 差分，首帧丢弃。

### 4.3 内存

```text
/proc/meminfo: MemTotal, MemFree, MemAvailable, Buffers, Cached, SReclaimable,
               SwapTotal, SwapFree, Dirty, Shmem
used_pct = (MemTotal - MemAvailable) / MemTotal    // 与 GNOME/htop 口径一致
```

提供「是否把 cache 计入已用」的开关（TrafficMonitor 的口径是 `MemTotal - MemFree` 风格，用户会对比，所以做成可配置）。

### 4.4 磁盘 / GPU / 电池

| 指标 | 数据源 | 备注 |
|---|---|---|
| 读写速率 | `/proc/diskstats` 扇区数 × 512，差分 | 过滤 `loop*`/`ram*`/`sr*`；只取整盘或按挂载点 |
| 分区使用率 | `statvfs(2)` | |
| AMD GPU | `/sys/class/drm/card*/device/gpu_busy_percent`、`mem_info_vram_used` | 免特权 |
| NVIDIA GPU | `nvidia-smi --query-gpu=... --format=csv,noheader,nounits` | 或 NVML（`nvml-wrapper`） |
| Intel GPU | `gt_cur_freq_mhz` + `/proc/<pid>/fdinfo` 的 `drm-engine-*` 累计时间 | 复杂，M4 再做 |
| 电池 | `/sys/class/power_supply/BAT*/{capacity,status}` | |

### 4.5 进程级流量（差异化功能）

| 方案 | 原理 | 特权 | 精度 | 开销 | 结论 |
|---|---|---|---|---|---|
| A. AF_PACKET 抓包 | `socket(AF_PACKET, SOCK_RAW)` 抓包 → 五元组 → `/proc/net/{tcp,udp}` inode → 扫 `/proc/*/fd` 建 pid 映射 | 需 `CAP_NET_RAW` | 中（丢包则偏差） | 高（用户态解析每个包） | 兼容性最好，nethogs 同款 |
| B. eBPF | kprobe `tcp_sendmsg`/`tcp_cleanup_rbuf` 或 tracepoint `sock:inet_sock_set_state`，按 pid/cgroup 累加到 map | 需 `CAP_BPF`+`CAP_PERFMON` | 高 | 极低 | **推荐**，用 `aya`（纯 Rust，无 libbpf） |
| C. cgroup v2 | `BPF_PROG_TYPE_CGROUP_SKB` 按 cgroup 统计 | 需 root | 高（无 pid 粒度） | 极低 | 容器场景补充 |

**建议**：默认关闭；检测到 `CAP_BPF` 时启用 eBPF 模式，否则提示「可安装 systemd 提权服务」或降级到方案 A。
在 Wayland/普通桌面下，进程级流量只是附加页，不阻塞 MVP。

---

## 5. 展示层设计

### 5.1 渲染技术选型

| 方案 | 系统依赖 | 透明/无边框 | 皮肤可控性 | 包体积 | 评价 |
|---|---|---|---|---|---|
| **Rust 自绘**：tiny-skia + cosmic-text + x11rb / wayland-client | 无 | 完全可控 | 完全可控 | ~10MB 静态 | **推荐** |
| egui / iced / slint | 无 | 需自接窗口层 | 中 | 10-20MB | 可接受，但排版/DPI 细节不如自绘可控 |
| GTK4 + libadwaita | GTK4 运行库 | Wayland 下不可定位 | 差 | 小 | 只用于「设置界面」 |
| Qt 6 | Qt 运行库（~100MB） | 好 | 好 | 大 | 违背零依赖 |
| Tauri/Electron | WebKitGTK / Chromium | 好 | 好 | 很大 | 不推荐 |

**结论**：主程序自绘；设置界面先用 TOML + CLI，后续可选独立 `trafficview-settings`（GTK4）而不影响主程序依赖。

### 5.2 悬浮窗（主窗口）

| 会话 | 实现 | 关键点 |
|---|---|---|
| X11 | `override_redirect = true` 顶层窗口，ARGB visual（32 位 depth），`XRender`/`SHM` 提交 | 自己处理 RandR 热插拔、多屏坐标、`Xft.dpi` 缩放 |
| Wayland (wlroots/KDE) | `zwlr_layer_surface_v1`，`layer=overlay`，`anchor=top|right` + margin，`exclusive_zone=-1`（不挤占面板） | 需要 `wl_shm` + `fractional-scale-v1`；无键盘交互 |
| GNOME/Wayland | 普通 `xdg_toplevel`（`set_app_id("trafficview")`），位置不可控 | 降级：只保证尺寸/透明度/左键拖动；「贴边常显」改由 Shell 扩展实现。本仓库实现：存在 Xwayland（`DISPLAY`）时优先改用 X11 override-redirect 窗口，位置可持久化 |

**鼠标穿透**：
- X11：`XShape` 输入区域置空 或 `XTestFakeInput` 无关；更简单是设置 `override_redirect` + 空 input shape。
- Wayland：`wl_surface.set_input_region(empty)`。

**拖拽与吸附**：X11 直接改坐标；Wayland layer-shell 通过改 margin 实现像素级移动（可做「吸附到边角 + 8px 网格」）；GNOME 的 `xdg_toplevel` 降级窗口在左键按下时用按钮 serial 调用 `xdg_toplevel.move`，由合成器接管交互式拖动。

**多显示器**：layer-shell 每个 output 一个 surface；X11 按 RandR 输出矩形映射。

### 5.3 托盘图标（SNI）

- 用 `ksni`（纯 Rust 实现 KDE/freedesktop StatusNotifierItem，Tokio 或 async-io 运行时可选）。
- 图标：`tiny-skia` 把 `↑1.2M ↓8.4M` 之类渲染成 ARGB32 位图，作为 `IconPixmap` 随属性更新（SNI 支持直接传像素数组，不落盘）。
- 尺寸自适应：读取 panel 的 `IconThemePath`/`PanelIconSize` 提示，默认 22/24px；用 2x 位图适配 HiDPI。
- 兼容性：KDE、XFCE、Cinnamon、MATE、Budgie、Ubuntu 原生可用；GNOME 需 AppIndicator 扩展（Ubuntu 默认开）。
- 交互：左键 `Activate` 切换悬浮窗显隐；右键 `ContextMenu`（显示设置项开关、网卡选择、暂停、设置、退出）。

### 5.4 顶栏文字（最接近「任务栏显示」的体验）

| 桌面 | 做法 |
|---|---|
| **GNOME 45+** | 自研 Shell 扩展（GJS/ESM）：`Gio.DBusProxy` 订阅 `com.trafficview.Daemon1` 的 `Sample` 信号 → `St.Label` 插入 `Main.panel.statusArea` 右侧 `_rightBox`；点击弹 `PopupMenu` |
| **KDE Plasma 6** | 自研 plasmoid（QML），`Plasma5Support.DataSource` 或 `DBusInterface` 取数据，放在 panel 里 |
| **Sway/Hyprland/i3/waybar** | `custom/<name>` 模块，`exec = trafficview status --format waybar`，每 1s 轮询 |
| **其他** | 托盘图标 + 悬浮窗 |

> 附带提供 `trafficview status --json` / `--format waybar|polybar|tmux`，让所有「自定义面板」都能零成本接入。这是 Linux 版的通用逃生舱。

### 5.5 皮肤系统

自研 `skin.toml` + 背景图，模型对齐 `skin.ini`：

```toml
[meta]
name = "Default Dark"
author = "trafficview"
version = "1.0"

[canvas]
background = "background.png"     # 支持透明 PNG
nine_patch = [8, 8, 8, 8]         # 可选九宫格拉伸，适配不同宽度
padding = [10, 6, 10, 6]

[[item]]
id      = "net_up"
rect    = [10, 6, 92, 20]         # 相对画布 x,y,w,h
font    = "Noto Sans CJK SC"
size    = 13.0
color   = "#4CAF50"
align   = "right"
format  = "↑ {value}"
mono    = true                    # 等宽数字

[[item]]
id      = "net_down"
rect    = [10, 26, 92, 20]
color   = "#2196F3"
format  = "↓ {value}"

[[item]]
id      = "cpu"
rect    = [110, 6, 80, 20]
format  = "CPU {value}%"
thresholds = [[80, "#FFC107"], [90, "#F44336"]]   # 阈值变色
```

- 内置 item id：`net_up / net_down / cpu / mem / swap / disk_read / disk_write / gpu / temp_cpu / temp_gpu / uptime / plugin.<id>`。
- 提供 `trafficview skin convert <trafficmonitor-skin-dir>` 把 `skin.ini`/`skin.xml` + 背景图转成 `skin.toml`（可选，M5）。
- 深浅色自动切换：监听 `org.freedesktop.appearance color-scheme`（D-Bus portal）或环境变量，切 `background_l.png`。

### 5.6 设置

- 一期：`~/.config/trafficview/config.toml` + `trafficview config get/set` + `trafficview config edit`（调 `$EDITOR`）。
- 二期：可选 GTK4 设置窗口（独立进程，通过 D-Bus 读写 daemon 配置）。
- 所有配置变更走 `ReloadConfig()` 热加载，不重启 daemon。

---

## 6. 插件系统

| 方案 | 语言无关 | 崩溃隔离 | ABI 稳定性 | 性能 | 结论 |
|---|---|---|---|---|---|
| **子进程 + JSON/stdio** | ✅ | ✅ | ✅ | 够用（1Hz） | **推荐** |
| C ABI `.so` | ❌（须 C/Rust） | ❌ | ❌（结构体布局） | 最好 | 高性能插件备用 |
| WASM（wasmtime） | ✅（多语言编译到 wasm） | ✅ | ✅ | 好 | 安全沙箱，二期 |
| D-Bus 服务 | ✅ | ✅ | ✅ | 中 | 适合重插件 |

**子进程协议（`trafficview-plugin-v1`）**：行分隔 JSON over stdin/stdout。

```jsonc
// daemon → plugin（每周期）
{"t": 1734567890.123, "interval_ms": 1000}

// plugin → daemon
{
  "items": [
    {"id": "gpu_load", "name": "GPU", "unit": "%", "format": "{value}%"},
    {"id": "fan_rpm",  "name": "FAN", "unit": "rpm"}
  ],
  "values": {"gpu_load": "37", "fan_rpm": "1420"}
}
```

- 插件声明 `items` 后即可被加入显示项列表、皮肤布局、告警规则（与内置项完全同权）。
- 超时（>2s）自动重启并禁用，带退避；stderr 直接进日志。
- 插件发现路径：`~/.local/share/trafficview/plugins/*/plugin.toml`（`command`、`interval_ms`、`capabilities`）。

---

## 7. 配置 / 持久化 / 告警

**目录（XDG）**

```
~/.config/trafficview/config.toml        主配置（含皮肤选择、显示项、网卡规则）
~/.config/trafficview/skins/<name>/     用户皮肤
~/.local/share/trafficview/history.db   SQLite 历史流量
~/.local/state/trafficview/state.json   运行时状态（窗口位置、最后累计值）
```

**SQLite schema（rusqlite `bundled`，静态编译，无系统依赖）**

```sql
CREATE TABLE iface(
  id       INTEGER PRIMARY KEY,
  stable   TEXT UNIQUE,        -- MAC 或 ifindex+name 的稳定键
  name     TEXT NOT NULL,
  last_rx  INTEGER, last_tx INTEGER
);
CREATE TABLE traffic_min(
  iface_id INTEGER, ts INTEGER, rx INTEGER, tx INTEGER,
  PRIMARY KEY(iface_id, ts)
) WITHOUT ROWID;
CREATE TABLE traffic_day(
  iface_id INTEGER, ymd TEXT, rx INTEGER, tx INTEGER,
  PRIMARY KEY(iface_id, ymd)
) WITHOUT ROWID;
```

- 每分钟聚合一行，按日 rollup，明细保留 90 天（可配置）。
- 网卡重命名用 MAC + ifindex 定位，避免统计断裂。
- 重启后从 `last_rx/last_tx` 继续累计；若计数器变小（重启/重置）则以当前值为新基线。

**告警**
- 规则：网速超阈值（持续 N 秒）、CPU/内存持续超阈值、月流量超配额、连接断开。
- 通知走 `org.freedesktop.Notifications`（`notify-rust`），带冷却时间（默认 5 分钟）与静默时段。
- 告警状态机独立于渲染，托盘/悬浮窗可通过 D-Bus `Alert` 信号同步变色。

---

## 8. 技术选型汇总

| 层 | 选型 | 理由 |
|---|---|---|
| 语言 | Rust 2021+（stable） | 单二进制、无 GC 抖动、`unsafe` 可控、生态覆盖 D-Bus/渲染/eBPF |
| 异步/定时 | `tokio`（或 `smol`）+ `timerfd` | 对齐秒边界，减少唤醒 |
| D-Bus | `zbus` | 纯 Rust，不需要 `libdbus` |
| 托盘 | `ksni` | 纯 Rust SNI，无 GTK |
| 窗口 (X11) | `x11rb` | 纯 Rust XCB 绑定 |
| 窗口 (Wayland) | `wayland-client` + `wayland-protocols-wlr` | layer-shell / fractional-scale |
| 2D 绘制 | `tiny-skia` | CPU 光栅化，png 解码即贴图，透明通道精确 |
| 文字 | `cosmic-text` + `fontdb` | 支持 CJK、fallback、subpixel、等宽数字 |
| 图像 | `image` (png) | 皮肤背景 |
| 配置 | `serde` + `toml` | |
| 存储 | `rusqlite`(bundled) | 静态 SQLite，零系统依赖 |
| 通知 | `notify-rust` | |
| 日志 | `tracing` + `tracing-subscriber` | 按级别写 journald/syslog |
| CLI | `clap` | |
| eBPF（可选） | `aya` | 纯 Rust，无 libbpf 依赖 |
| 插件宿主 | `std::process` + `serde_json` | |
| 打包 | `cargo-deb` / `cargo-generate-rpm` / AppImage | |

**产物**：`trafficviewd`（daemon）、`trafficview`（CLI/单实例控制）、`trafficview-tray`、`trafficview-overlay`。
前端可拆进程，也可合并为一个二进制用子命令（`trafficview tray|overlay`）以减少打包复杂度——**推荐合并**。

---

## 9. 性能与资源预算

| 项 | 目标 | 手段 |
|---|---|---|
| CPU 占用 | < 0.3%（1s 采样，无 eBPF） | 一次 syscall 读全量；无变化不重绘 |
| 内存 RSS | < 30 MB | 无 Web 运行时；皮肤位图按需加载 |
| 唤醒 | 1 次/秒 | `timerfd` + `epoll` 统一事件循环 |
| 启动 | < 80 ms | 静态二进制，无动态初始化 |
| eBPF 模式 | < 1% CPU | map 采样而非每包用户态回调 |

**渲染节流**：仅当格式化后的文本或图标像素变化时才提交 surface；Wayland 等 `frame` 回调，X11 限 10fps 上限。

---

## 10. 打包与权限

| 分发形式 | 覆盖 | 限制 |
|---|---|---|
| `.deb` / `.rpm` | Debian/Ubuntu/Fedora/openSUSE | 推荐主渠道 |
| AppImage | 全发行版 | glibc 兼容；体积大 |
| AUR | Arch | |
| Flatpak | 通用 | 沙箱看不到完整 `/proc`，**eBPF/进程流量不可用**，仅基础指标；不建议作为主渠道 |

**systemd 集成**

```ini
# trafficviewd.service (--user)
[Unit]
Description=TrafficView metrics daemon
After=graphical-session.target
PartOf=graphical-session.target

[Service]
Type=dbus
BusName=com.trafficview.Daemon1
ExecStart=/usr/bin/trafficviewd
Restart=on-failure
RestartSec=3
```

- eBPF 提权（可选）：单独的 system 级 `trafficviewd-ebpf.service`，仅授予 `CapabilityBoundingSet=CAP_BPF CAP_PERFMON CAP_NET_ADMIN` + `NoNewPrivileges=yes` + `ProtectSystem=strict`，通过 D-Bus 把聚合结果推给用户态 daemon。
- 自启动：`~/.config/autostart/trafficview.desktop`（XDG autostart，跨 DE 可用）。
- 单实例：D-Bus name 注册失败即 `Activate` 已有实例并退出。

---

## 11. 代码结构

```
trafficview/
├── Cargo.toml                  # workspace
├── crates/
│   ├── core/                   # 指标模型、Snapshot、格式化、单位换算、平滑
│   ├── collectors/
│   │   ├── net.rs              # rtnetlink + /proc/net/dev 兜底
│   │   ├── cpu.rs              # /proc/stat, cpufreq
│   │   ├── mem.rs              # /proc/meminfo
│   │   ├── disk.rs             # /proc/diskstats, statvfs
│   │   ├── hwmon.rs            # /sys/class/hwmon
│   │   ├── gpu.rs              # drm sysfs / nvidia-smi
│   │   └── ebpf/               # 可选 feature
│   ├── daemon/                 # 调度、zbus 服务、SQLite、告警、插件宿主
│   ├── render/                 # tiny-skia 画布、皮肤解析、item 布局、文字排版
│   ├── ui-x11/                 # override-redirect 窗口
│   ├── ui-wayland/             # layer-shell 窗口
│   ├── tray/                   # ksni
│   └── plugin-proto/           # 行分隔 JSON 协议类型
├── frontends/
│   ├── gnome-extension/        # GJS，metadata.json + extension.js
│   └── plasma-widget/          # QML plasmoid（可选）
├── packaging/
│   ├── deb/  rpm/  appimage/
│   └── systemd/
└── docs/
```

---

## 12. 里程碑

| 阶段 | 范围 | 验收 |
|---|---|---|
| **M0 PoC** | 读 `/proc/net/dev` 打印速率；`/proc/stat` 打印 CPU | 与 `nload`/`htop` 读数误差 < 3% |
| **M1 MVP** | daemon + D-Bus + X11 悬浮窗（固定皮肤、网速/CPU/内存）+ 托盘图标 + TOML 配置 + autostart | 在 XFCE/X11 下连续运行 24h 无泄漏 |
| **M2 展示完善** | 皮肤系统（PNG/九宫格/阈值变色）+ Wayland layer-shell + GNOME Shell 扩展 + `--format waybar` | KDE-Wayland 与 GNOME 45+ 均可用 |
| **M3 数据能力** | 历史流量 SQLite + 日月统计 + 告警通知 + 多网卡自动选择 + 深色自动切换 | 重启/休眠后统计连续 |
| **M4 进阶** | 进程级流量（eBPF + 降级）+ GPU/温度 + 插件宿主 + 设置界面 | 权限缺失时优雅降级 |
| **M5 发布** | deb/rpm/AppImage + 文档 + 皮肤转换器 + 基准测试 | 主流发行版安装即用 |

---

## 13. 风险与对策

| 风险 | 概率 | 影响 | 对策 |
|---|---|---|---|
| GNOME 不支持 layer-shell，悬浮窗无法贴边常显 | 高 | 核心体验打折 | GNOME 走 Shell 扩展（顶栏文字），悬浮窗降级为普通窗口并明示支持矩阵 |
| GNOME 默认隐藏托盘图标 | 高 | 用户以为没启动 | 首次启动给出引导（检测扩展、一键跳转安装）；同时提供扩展兜底 |
| 桌面环境碎片化（面板 API 各不相同） | 高 | 维护成本 | 以 D-Bus + `--format` 作为统一出口，只维护 GNOME 扩展 + KDE plasmoid 两个一等公民 |
| eBPF 需特权，发行版策略收紧 | 中 | 进程流量不可用 | 独立提权服务 + 明确降级；默认关闭该功能 |
| 休眠唤醒 / 计数器重置导致速率尖峰 | 中 | 数据失真 | 单调时钟 + 跳变检测 + 回绕处理 |
| VPN/虚拟网卡导致选错网卡 | 中 | 读数"不准" | 默认路由优先 + 可视化网卡列表（显示每卡实时速率供选择） |
| HiDPI 分数缩放文字模糊 | 中 | 观感 | 按物理像素渲染 + `fractional-scale-v1`；X11 读 `Xft.dpi` |
| 皮肤生态与 TrafficMonitor 不兼容 | 低 | 迁移成本 | 提供 `skin convert`；新皮肤格式保持"背景图 + rect 布局"的同构语义 |
| Flatpak 沙箱限制 | 中 | 分发受限 | 主推 deb/rpm/AppImage |

---

## 14. 需要产品侧确认的决策

1. **是否要求兼容 TrafficMonitor 皮肤包**（决定是否做 `skin.ini` 转换器与 1:1 布局语义）。
2. **GNOME 是否作为一等公民**（若是，Shell 扩展必须进 M2；若否，可只做托盘 + 普通窗口）。
3. **是否要进程级流量**（决定是否引入 eBPF 与提权服务，显著影响复杂度与审核成本）。
4. **是否需要 Web/远程查看**（若需要，daemon 可额外暴露 HTTP/SSE，但那会违背零依赖原则，建议独立可选模块）。

---

## 15. 参考

- [TrafficMonitor（GitHub）](https://github.com/zhongyang219/TrafficMonitor)
- [TrafficMonitor README（Gitee 镜像）](https://gitee.com/zhongyang219/TrafficMonitor)
- [StatusNotifierItem 规范](https://www.freedesktop.org/wiki/Specifications/StatusNotifierItem/)
- [ksni — Rust SNI 实现](https://crates.io/crates/ksni)
- [wlr-layer-shell 协议](https://wayland.app/protocols/wlr-layer-shell-unstable-v1)
- [Mutter 拒绝 layer-shell 的讨论（issue #1141）](https://gitlab.gnome.org/GNOME/mutter/-/issues/1141)
- [aya — 纯 Rust eBPF](https://aya-rs.dev/)
