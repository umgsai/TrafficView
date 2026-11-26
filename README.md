# TrafficView

Linux 桌面实时流量与系统负载监控。核心是一个只读 `/proc`、`sysfs` 与 netlink 的守护进程，
通过 D-Bus 对外提供数据；悬浮窗、托盘、命令行和设置界面都只订阅 D-Bus，不各自读系统文件。

## 当前能力

- **悬浮窗**（X11）：上下行速率、CPU、内存、CPU 温度等显示项；可拖动、悬停穿透、调透明度、置顶，
  皮肤用 TOML 描述，支持阈值变色。
- **托盘**（StatusNotifierItem + DBusMenu）：暂停/恢复、显示/隐藏悬浮窗、切换网卡、
  打开设置界面或配置文件、退出。
- **历史统计**：SQLite 按分钟、日、月聚合，`trafficview history today|month`。
- **告警**：阈值 + 持续时间 + 冷却时间 + 静默时段，触发后发送桌面通知。
- **多前端输出**：`text`、`json`、`waybar`、`polybar`、`tmux`。
- **设置界面**：GTK4 + libadwaita 独立程序，覆盖通用、网络、显示、告警、历史、插件六个页面。

采集来源：netlink `IFLA_STATS64`（网速）、`/proc/stat`（CPU）、`/proc/meminfo`（内存）、
`/proc/diskstats`、`/proc/mounts`、`/proc/uptime`，以及 sysfs 中的电池、温度、GPU。
不可用的采集器不会伪造零值，而是通过 `CollectorStatus` 返回 `available=false` 与原因。

## 截图

![TrafficView 悬浮窗与托盘](docs/images/overlay.png)

浅色壁纸下悬浮窗会按背景自动加深文字，托盘使用固定应用图标：

![托盘 tooltip](docs/images/tray-tooltip.png)

## 依赖

- Linux（`/proc`、`/sys`、netlink、D-Bus 会话总线）
- Rust stable（本仓库在 1.100.0 上验证）
- 只有构建设置界面需要 GTK 开发包：

```bash
sudo apt install libgtk-4-dev libadwaita-1-dev   # Debian/Ubuntu
```

## 构建与打包

当前提供 Debian 包；rpm/AppImage 尚未实现。

```bash
./packaging/build-deb.sh
sudo dpkg -i dist/trafficview_1.0.2-1_amd64.deb
```

`build-deb.sh` 依次执行 fmt、clippy、测试（含独立的设置界面 workspace）与 release 构建，
再组装单个主包并写出 `dist/SHA256SUMS`。包内包含三个程序、systemd user 单元、会话
自启动条目、桌面入口、图标与文档，不含 eBPF/AF_PACKET 特权 helper。安装后 daemon、
托盘与悬浮窗（仅 X11 会话）由 `/usr/libexec/trafficview-session` 在登录时启动；
也可以 `systemctl --user enable --now trafficviewd` 把 daemon 交给 systemd 托管。
发行说明见 [docs/releases/1.0.2.md](docs/releases/1.0.2.md)。

只用源码运行：

```bash
cargo build --workspace --release
cargo build --manifest-path frontends/settings/Cargo.toml
```

产物：

- `target/release/trafficview`：命令行与各前端入口
- `target/release/trafficviewd`：守护进程
- `frontends/settings/target/release/trafficview-settings`：设置界面（独立 workspace，
  不参与根目录的 release 构建）

发布构建脚本（仅二进制与校验和，不生成 deb）：

```bash
./packaging/build-release.sh
```

### 在 arm64 上构建

`build-deb.sh` 用 `dpkg --print-architecture` 决定包架构，在 arm64 机器上直接构建即可，
产物为 `dist/trafficview_1.0.2-1_arm64.deb`：

```bash
sudo apt install build-essential pkg-config dpkg-dev libgtk-4-dev libadwaita-1-dev
./packaging/build-deb.sh
```

说明：

- 主程序是纯 Rust；唯一需要现场编译的 C 代码是 `rusqlite` 的 bundled SQLite，arm64 的
  `build-essential` 已足够。
- 设置界面依赖 GTK4/libadwaita。交叉编译需要对应的 arm64 sysroot 与 pkg-config 信息，
  推荐在 arm64 设备、arm64 容器或用 `qemu` 的 chroot 里原生构建；包的依赖由
  `dpkg-shlibdeps` 从本地二进制推导，无需手写。
- 悬浮窗仅支持 X11。arm64 设备通常运行 Wayland 会话，此时 daemon、命令行与托盘可用，
  但不会自动出现悬浮窗。

## 运行

```bash
./target/release/trafficviewd &          # 守护进程，注册 com.trafficview.Daemon1
./target/release/trafficview overlay &   # X11 悬浮窗
./target/release/trafficview tray &      # 托盘图标
./target/release/trafficview settings    # 设置界面，也可以从托盘右键菜单打开
```

守护进程与前端必须处于同一个 D-Bus 会话总线。悬浮窗是否显示由配置 `overlay.visible` 决定，默认显示。

设置界面按“与 `trafficview` 同目录 → `PATH`”查找。源码构建时两者不在同一目录，可建软链：

```bash
ln -sf ../../frontends/settings/target/debug/trafficview-settings target/release/trafficview-settings
```

## 常用命令

```console
$ trafficview status
iface=enp0s3 ↓60 B/s ↑60 B/s CPU 51% MEM 27% ts=426882ms

$ trafficview status --format json
{"ts_ms":426882,"net":{"iface":"enp0s3","rx_bps":60,"tx_bps":60,"rx_total":23283968,"tx_total":9585290},"cpu":{"total_pct":51.21043,...

$ trafficview status --format waybar
{"alt":"normal","class":"normal","text":"↓21.5 KB/s ↑73.9 KB/s CPU 66% MEM 24%","tooltip":"TrafficView\niface: enp0s3\nDownload: 21.5 KB/s\n..."}

$ trafficview interfaces
enp0s3       mac:08:00:27:fc:22:08    up       carrier=true active=true
lo           name:lo                  unknown  carrier=true active=false

$ trafficview history today
day 2026-11-23..2026-11-23  rx 16.6 KB  tx 27.7 KB
  enp0s3 (mac:08:00:27:fc:22:08)  rx 16.6 KB  tx 27.7 KB

$ trafficview pause
Pause ok
$ trafficview status
daemon is paused        # 退出码 2
$ trafficview resume
Pause ok
```

（JSON 与 waybar 输出为单行，上面做了截断；实际字段随采集器可用性变化。）

| 子命令 | 作用 |
| --- | --- |
| `status [--format text\|json\|waybar\|polybar\|tmux]` | 查询当前快照 |
| `interfaces` | 列出网卡、稳定键、状态与是否被选中 |
| `history today\|month [--iface <stable>]` | 查询今日/本月流量 |
| `pause` / `resume` | 暂停/恢复采样；暂停时 `status` 退出码为 2 |
| `config path\|get\|set\|edit` | 配置文件路径、读取、修改、编辑 |
| `skin validate\|preview` | 校验皮肤，或用快照 JSON 离屏渲染 PNG |
| `overlay` / `tray` / `settings` | 启动对应前端 |
| `autostart enable\|disable\|status` | 管理 XDG 自启动条目 |
| `quit` | 让守护进程退出 |

## 配置

```console
$ trafficview config path
/home/shang/.config/trafficview/config.toml
```

- `config get` 打印规范化后的完整配置。
- `config set <key> <value>` 校验通过后写回文件，再请求守护进程重新加载；校验失败不会落盘。
- `config edit` 使用 `$VISUAL` / `$EDITOR` 编辑，退出后同样先校验再保存。
- 设置界面不走文件，而是通过 D-Bus 的 `UpdateConfig` 整体提交，由守护进程校验并原子保存。

主要字段：

| 字段 | 含义 |
| --- | --- |
| `sample_interval_ms` | 采样间隔，500–30000，默认 1000 |
| `active_interface` | 指定网卡名；留空表示按黑白名单自动选择 |
| `interface_blacklist` / `interface_whitelist` | 自动选择时的网卡过滤规则，支持通配符 |
| `network_smoothing` | `none` / `moving_average_3` / `ema` |
| `theme` | `auto` / `light` / `dark` |
| `[history] retention_days` / `timezone` | 历史保留天数与时区 |
| `[alerts] rules` | 告警规则：指标、比较符、阈值、持续时间、冷却、静默时段、级别 |
| `overlay.cpu_temp_sensor` | CPU 温度传感器 label；留空自动识别，例如 `k10temp/Tdie` |
| `[overlay]` | `visible`、`position`、`opacity`、`click_through`、`always_on_top`、`skin`、`units`、`items` |
| `[plugins] enabled` | 插件宿主开关 |
| `[process_traffic] backend` | `off` / `ebpf` / `af_packet` / `auto` |

CPU 温度显示项 id 为 `cpu_temp`。新生成的配置文件默认已包含该项；已有配置文件不会被自动改写，
需要手动在 `[[overlay.items]]` 中追加：

```toml
[[overlay.items]]
id = "cpu_temp"
label = "TEMP"
unit = "°C"
format = "{value}"
color = "#FFA94D"
visible = true
```

检测不到 CPU 传感器时该项自动隐藏。需要手动指定传感器时，可执行
`trafficview config set overlay.cpu_temp_sensor k10temp/Tdie`，或在 `[overlay]` 中写
`cpu_temp_sensor = "..."`。只写 chip 名（如 `k10temp`）会按 label 排序命中第一个匹配项，
建议写完整 label（如 `k10temp/Tdie`）。

自启动：

```console
$ trafficview autostart enable
/home/shang/.config/autostart/trafficview.desktop
$ trafficview autostart status
enabled /home/shang/.config/autostart/trafficview.desktop
```

安装 deb 时还会写入系统级条目 `/etc/xdg/autostart/trafficview-session.desktop`，登录后由
`/usr/libexec/trafficview-session` 拉起 daemon、托盘与悬浮窗（X11）。托盘和悬浮窗使用固定
总线名做单实例保护，与用户级条目重复启动时不会出现第二份。

## D-Bus 接口

- 总线名：`com.trafficview.Daemon1`
- 对象路径：`/com/trafficview/Daemon1`
- 接口名：`com.trafficview.Daemon1`

方法：`GetSnapshot`、`GetConfig`、`UpdateConfig`、`GetInterfaceDetails`、`GetTrafficSummary`、
`SetActiveInterface`、`SetInterval`、`SetOverlayVisible`、`ReloadConfig`、`Pause`、`Quit`。

信号：`Sample(a{sv})`、`Alert(sss)`、`InterfacesChanged(as)`、`ConfigChanged()`。

只读属性：`Interfaces`、`ActiveInterface`、`CollectorStatus`、`ColorScheme`、`Paused`、`SampleInterval`。

```console
$ gdbus call --session -d com.trafficview.Daemon1 \
    -o /com/trafficview/Daemon1 -m com.trafficview.Daemon1.GetConfig
({'memory_include_cache': <false>, 'active_interface': <''>, 'sample_interval_ms': <uint32 1000>, 'theme': <'light'>, ...
```

完整契约见 [docs/dbus-v1.md](docs/dbus-v1.md)。

## 已验证的范围

以下在 VirtualBox + Cinnamon/X11（单网卡 `enp0s3`、无 GPU）与 Deepin 25 / amd64 环境里实机验证过：

- 守护进程采样、D-Bus 调用、历史写入与跨重启累计
- 悬浮窗显示/隐藏、位置、透明度、穿透、皮肤热更新与无效皮肤降级
- 托盘全部菜单项（Cinnamon 通过 `EventGroup` 投递点击）
- 设置界面读写配置、断连横幅降级、自动重连
- 告警触发并送达桌面通知
- 无电池、无温度传感器、无 GPU 时按 `CollectorStatus` 降级，不出现伪造零值
- Debian 主包（1.0.2）：Deepin 25 / amd64 上构建、`dpkg -i` 安装，启动菜单、会话自启动、
  daemon 与设置界面均可用

仍需换环境验证：KDE Plasma / GNOME 的 Wayland 会话、waybar 实际渲染、XFCE 下 24 小时运行、
GPU 读数、eBPF 后端、网卡热插拔、休眠唤醒后的统计连续性、rpm/AppImage 的打包。

性能现状与后续计划见 [docs/project-status.md](docs/project-status.md)。

## 目录结构

```
crates/core         采样模型、配置、格式化、单调时钟
crates/collectors   /proc、sysfs、netlink 采集
crates/daemon       D-Bus 服务、历史库、告警、主题
crates/cli          命令行与各前端入口
crates/tray         StatusNotifierItem 托盘
crates/ui-x11       X11 悬浮窗
crates/render       皮肤解析与离屏渲染
frontends/settings  GTK4 设置界面（独立 workspace）
packaging           systemd 单元、会话启动器、桌面/自启动条目、图标、deb 与发布构建脚本
docs                路线图、D-Bus 契约、项目状态、发行说明
```
