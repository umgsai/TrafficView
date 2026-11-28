# TrafficView

Linux 桌面实时流量与系统负载监控。核心是一个只读 `/proc`、`sysfs` 与 netlink 的守护进程，
通过 D-Bus 对外提供数据；悬浮窗、托盘、命令行和设置界面都只订阅 D-Bus，不各自读系统文件。

## 当前能力

- **悬浮窗**（X11；Wayland 下 KDE/wlroots 走 layer-shell，GNOME 优先经 Xwayland
  使用 X11 窗口，缺少 Xwayland 时降级为普通窗口）：
  上下行速率、CPU（可内联温度）、内存、GPU（可内联温度）等显示项；
  可拖动、悬停穿透、调透明度、置顶，皮肤用 TOML 描述，支持阈值变色。
- **托盘**（StatusNotifierItem + DBusMenu）：暂停/恢复、显示/隐藏悬浮窗、切换网卡、
  打开设置界面或配置文件、退出。
- **历史统计**：SQLite 按分钟、日、月聚合，`trafficview history today|month`。
- **告警**：阈值 + 持续时间 + 冷却时间 + 静默时段，触发后发送桌面通知。
- **多前端输出**：`text`、`json`、`waybar`、`polybar`、`tmux`。
- **设置界面**：GTK4 + libadwaita 独立程序，覆盖通用、网络、显示、告警、历史、插件六个页面。
- **中英文界面**：`language = "auto" | "zh" | "en"`，覆盖托盘菜单与 tooltip、告警通知
  与设置界面；默认跟随系统环境。

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

## 发行版支持

本仓库仅在以下发行版上验证通过：

- **Deepin 25**（amd64，本机日常使用与构建）
- **Linux Mint 22.3**（amd64，Cinnamon/X11）：1.1.1 包实机安装运行通过；
  1.2.0 源码构建与质量门通过，悬浮窗乱码修复经渲染复核
- **Ubuntu 26.04 LTS**（amd64，GNOME 50.1 / Wayland，VirtualBox）：1.2.0 包
  安装运行通过；悬浮窗经 Xwayland 使用 X11 后端，显示、拖动、退出重开后位置恢复，
  会话自启动、托盘、设置界面均正常。1.2.1 收编同一组 Wayland/GNOME 修复

deb 包硬依赖：`libc6 (>= 2.35)`、`libglib2.0-0t64 (>= 2.54.0)`（t64 命名，
Ubuntu 24.04+ / Debian 13+）、`libgtk-4-1 (>= 4.0.0)`、
`libadwaita-1-0 (>= 1.4~beta)`。其它发行版的实际状态：

- **Ubuntu 24.04 LTS**：依赖满足、可以安装；此前记录的运行异常尚未在 24.04
  上复测，建议使用包含 Wayland 悬浮窗自启动/拖动修复的最新 1.2.1 包。
- **Ubuntu 22.04 LTS**：libc6 满足（2.35），但 libadwaita 只有 1.2.0，
  设置界面二进制过不去；核心功能（悬浮窗 / 托盘 / CLI）理论上能跑，但
  当前打包脚本把所有二进制打成一个包，被 GTK4 依赖卡死。
- **Debian 12 Bookworm**：libc6 2.36 满足，但 GTK4 / libadwaita 版本偏旧。
- **Ubuntu 20.04 LTS / Debian 11 Bullseye**：libc6 也不够（2.31 / 2.31）。

如果你只需要悬浮窗 + 托盘 + CLI，不需要 GTK4 设置界面，后续版本计划拆分
成两个 deb，让核心包在 Ubuntu 22.04+ 都能直接安装。

## 构建与打包

当前提供 Debian 包；rpm/AppImage 尚未实现。

```bash
./packaging/build-deb.sh
sudo dpkg -i dist/trafficview_1.2.1-1_amd64.deb
```

`build-deb.sh` 依次执行 fmt、clippy、测试（含独立的设置界面 workspace）与 release 构建，
再组装单个主包并写出 `dist/SHA256SUMS`。包内包含三个程序、systemd user 单元、会话
自启动条目、桌面入口、图标与文档，不含 eBPF/AF_PACKET 特权 helper。安装后 daemon、
托盘与悬浮窗由 `/usr/libexec/trafficview-session` 在登录时启动（X11 与 Wayland 会话均会
拉起悬浮窗）；也可以 `systemctl --user enable --now trafficviewd` 把 daemon 交给 systemd 托管。
发行说明见 [docs/releases/1.2.1.md](docs/releases/1.2.1.md)。

### 卸载

先停止运行中的程序，再卸载软件包：

```bash
pkill -f '/usr/bin/trafficview-settings'
pkill -f '/usr/bin/trafficview tray'
pkill -f '/usr/bin/trafficview overlay'
systemctl --user disable --now trafficviewd 2>/dev/null
pkill -x trafficviewd
sudo apt remove trafficview
```

如需连 `/etc` 下的包级配置一起清除，把最后一行换成 `sudo apt purge trafficview`。

卸载会保留个人数据：`~/.config/trafficview/config.toml`（配置，含自定义显示样式）
和 `~/.local/share/trafficview`（历史统计，如存在）。保留配置后，重装即可恢复原来的
显示样式；确认不再需要时可一并删除：

```bash
# 删除配置文件（自定义显示样式、主题、语言等都会丢失，重装后回到默认样式）
rm -rf ~/.config/trafficview
# 删除历史统计数据（SQLite 数据库，删除后不可恢复）
rm -rf ~/.local/share/trafficview
```

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
产物为 `dist/trafficview_1.2.1-1_arm64.deb`：

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
- 悬浮窗在 X11 使用 override-redirect 窗口；Wayland 下 KDE/wlroots 使用 layer-shell。
  GNOME/Mutter 既不支持 layer-shell，也不允许客户端设置坐标，因此存在 Xwayland
  （`DISPLAY` 可用）时自动改用 X11 后端，位置可跨重启恢复（置顶与穿透沿用 X11
  实现）；没有 Xwayland 时才降级为普通窗口（位置由合成器决定）。
  会话自启动会在 X11 与 Wayland 下都拉起悬浮窗。

## 运行

```bash
./target/release/trafficviewd &          # 守护进程，注册 com.trafficview.Daemon1
./target/release/trafficview overlay &   # 悬浮窗（X11 / Wayland 自动选择后端）
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
| `theme` | `auto` / `light` / `dark`，默认 `dark` |
| `language` | 界面语言：`auto`（默认，跟随系统）/ `zh` / `en`；覆盖设置界面、托盘与通知。设置界面切换语言后需重新打开窗口，托盘与通知立即生效 |
| `[history] retention_days` / `timezone` | 历史保留天数与时区 |
| `[alerts] rules` | 告警规则：指标、比较符、阈值、持续时间、冷却、静默时段、级别 |
| `overlay.cpu_temp_sensor` | CPU 温度传感器 label；留空自动识别，例如 `k10temp/Tdie` |
| `[overlay]` | `visible`、`position`、`opacity`、`click_through`、`always_on_top`、`skin`、`units`、`items` |
| `[plugins] enabled` | 插件宿主开关 |
| `[process_traffic] backend` | `off` / `ebpf` / `af_packet` / `auto` |

新生成的配置默认把温度内联显示：`cpu` 项为 `format = "{value}  {cpu_temp}"`，
`gpu` 项为 `format = "{value}  {gpu_temp}"`，即 `CPU 14%  70°C`、`GPU 12%  55°C`；
独立 `cpu_temp` 项保留在配置里但默认 `visible = false`。检测不到对应传感器时，
温度段自动省略；检测不到 GPU 时 `gpu` 整行隐藏。

已有配置文件不会被自动改写。若想让 CPU 温度单独占一行，把 `cpu` 项的 `format`
改回 `"{value}"`，并把 `cpu_temp` 项设为 `visible = true`；若配置里还没有 `gpu`
项，可手动追加：

```toml
[[overlay.items]]
id = "gpu"
label = "GPU"
unit = "%"
format = "{value}  {gpu_temp}"
color = "#B197FC"
visible = true
```

需要手动指定 CPU 传感器时，可执行
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
`/usr/libexec/trafficview-session` 拉起 daemon、托盘与悬浮窗（X11 与 Wayland）。托盘和悬浮窗使用固定
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

以下在 VirtualBox + Cinnamon/X11（单网卡 `enp0s3`、无 GPU）、Deepin 25 / amd64 与
Ubuntu 26.04 LTS（GNOME 50.1 / Wayland）环境里实机验证过：

- 守护进程采样、D-Bus 调用、历史写入与跨重启累计
- 悬浮窗显示/隐藏、位置、透明度、穿透、皮肤热更新与无效皮肤降级
- 托盘全部菜单项（Cinnamon 通过 `EventGroup` 投递点击）
- 设置界面读写配置、断连横幅降级、自动重连
- 告警触发并送达桌面通知
- 无电池、无温度传感器、无 GPU 时按 `CollectorStatus` 降级，不出现伪造零值
- Debian 主包（1.2.1）：Ubuntu 26.04 LTS / amd64 上构建、质量门通过；1.2.0 包在
  Deepin 25 上 `dpkg -i` 安装，启动菜单、会话自启动、daemon 与设置界面均可用
- 悬浮窗布局（1.2.1）：基准宽度 232px、双列 4:3 分配，超长文本列尾裁剪不再折行，
  两位数使用率与温度可同屏完整显示
- 中英文界面（托盘、通知、设置界面）与 `language` 配置项
- Linux Mint 22.3 下悬浮窗文字渲染修复（渲染输出复核，数字与字母恢复显示）
- Ubuntu 26.04 LTS（GNOME 50.1 / Wayland）：1.2.0 包安装运行正常；会话自启动拉起
  悬浮窗，左键可拖动；GNOME 存在 Xwayland 时自动使用 X11 后端，退出重开后按
  `overlay.position` 恢复位置，托盘与设置界面可用。1.2.1 包含同一组修复并通过构建与
  质量门
- Wayland 悬浮窗单实例保护（固定总线名 `com.trafficview.Overlay`，托盘菜单与自启动
  同时触发也只保留一个进程）
- GNOME 位置恢复（实测）：手动位置设为 300,200 后退出并重新打开，悬浮窗仍在 300,200

仍需换环境验证：KDE Plasma（Wayland）、waybar 实际渲染、XFCE 下 24 小时运行、
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
