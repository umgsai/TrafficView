# TrafficView 项目状态

> 最后更新：2026-11-24
> 当前里程碑：M0–M4 已完成（M2/M3 有少量真机项未覆盖），M5 进行中（Task 2 完成但性能预算未达标；Task 3 的 deb 部分完成，rpm/AppImage 未做）

## 一句话

采集、D-Bus 守护进程、X11 悬浮窗、托盘、GTK4 设置界面与历史/告警均已实现并在
Cinnamon/X11 实机跑通；版本已统一到 1.0.2，Debian 主包与发行说明已产出。剩余发布工程
是 daemon 稳态 CPU 0.400% 未达 0.3% 预算、rpm/AppImage，以及完整文档与提交。

## 里程碑状态

| 里程碑 | 状态 | 说明 |
| --- | --- | --- |
| M0 采集 PoC | 完成 | `/proc` 采集、1024 进制格式化、单调时钟差分 |
| M1 X11 MVP | 代码完成 | 悬浮窗 + 托盘 + D-Bus v1 契约冻结；计划复选框未勾（差 Wayland/其它桌面） |
| M2 显示与皮肤 | 代码完成 | Cinnamon/X11 15 项验收通过；KDE/GNOME/Wayland/waybar 未覆盖，完成条件未满足 |
| M3 历史/告警/网络 | 基本完成 | 两处真机项未验：网卡热插拔、重启/休眠/主题切换 |
| M4 高级指标 | 完成 | GPU/eBPF 无硬件实测；`process-ebpf` 为空占位 feature，`backend=ebpf` 恒降级 |
| M5 发布工程 | 进行中 | Task 2 完成（Step 4 未达标）；Task 3 完成 deb（rpm/AppImage 未做）；Task 4 完成 README/发行说明，专页文档未写；Task 1 未开始 |

## 1.0.2 发布物

- 版本：根 workspace 与 `frontends/settings` 均为 `1.0.2`
- 产物：`dist/trafficview_1.0.2-1_amd64.deb`（6.2 MB）与 `dist/SHA256SUMS`
- 打包方式：`packaging/build-deb.sh` + `dpkg-deb`，不依赖 debhelper/cargo-deb；
  依赖由 `dpkg-shlibdeps` 从暂存区二进制推导
- 包内容：`trafficview` / `trafficviewd` / `trafficview-settings` 三个程序、systemd
  user 单元、`/usr/libexec/trafficview-session` 会话入口、系统级自启动条目、两个桌面
  入口、hicolor 图标、README/dbus-v1/copyright/changelog
- 不包含：eBPF/AF_PACKET 特权 helper、对应 system 单元与 `com.trafficview.Ebpf1.conf`；
  选到这些后端时由 daemon 按不可用降级
- 1.0.2 相对 1.0.1：修复悬浮窗拖动不跟手（事件批次主动 flush、拖动时轮询间隔 1ms），
  内容未变化时也提交位置与穿透请求
- 1.0.1 相对 1.0.0：悬浮窗按背景自动调整文字对比度并加大字号/字重；修复托盘
  `Network interface` 子菜单返回重复根菜单；托盘图标改为内嵌的静态应用图标
- 发行说明：`docs/releases/1.0.2.md`（1.0.1 / 1.0.0 见 `docs/releases/`）

启动链：登录时 `/etc/xdg/autostart/trafficview-session.desktop` 调用
`/usr/libexec/trafficview-session`，依次确保 daemon 在跑、等待 `com.trafficview.Daemon1`
可用、X11 下拉起悬浮窗，最后前台运行托盘。托盘与悬浮窗分别持有
`com.trafficview.Tray` / `com.trafficview.Overlay` 固定总线名做单实例保护。未使用 D-Bus
激活文件，避免测试环境被系统级激活的 daemon 干扰。

## 代码结构

| 路径 | 作用 |
| --- | --- |
| `crates/core` | 模型、配置、格式化、时钟 |
| `crates/collectors` | `/proc`、sysfs、netlink、eBPF/AF_PACKET helper |
| `crates/daemon` | D-Bus 服务 `com.trafficview.Daemon1`、历史 SQLite、告警、主题 |
| `crates/cli` | `trafficview` 命令行（status/history/config/skin/overlay/tray/settings/autostart） |
| `crates/tray` | StatusNotifierItem + DBusMenu（固定总线名单实例） |
| `crates/ui-x11` | X11 悬浮窗（tiny-skia 软件渲染，固定总线名单实例） |
| `crates/render` | 皮肤解析与离屏渲染 |
| `frontends/settings` | 独立 `[workspace]` 的 GTK4/libadwaita 设置界面（根 `Cargo.toml` 用 `exclude` 排除，根构建不解析 GTK） |
| `packaging/` | systemd 单元、会话启动器、桌面/自启动条目、图标、`build-deb.sh`、`build-release.sh` |
| `docs/` | D-Bus 契约、项目状态、发行说明、路线图 |

## 怎么跑

```bash
# 构建
cargo build --workspace --release                 # CLI + daemon
cargo build --manifest-path frontends/settings/Cargo.toml   # 设置界面（独立 workspace）

# 打包（quality gate + release 构建 + deb）
./packaging/build-deb.sh

# 源码运行（需同一会话总线，DISPLAY=:0）
./target/release/trafficviewd &
./target/release/trafficview overlay &
./target/release/trafficview tray &
./target/release/trafficview settings            # 或托盘右键 → Settings…
```

设置界面查找顺序：与 `trafficview` 同目录 → `PATH`。本地调试需要软链：
`ln -sf ../../frontends/settings/target/debug/trafficview-settings target/release/trafficview-settings`（`target/` 不入库，打包时两二进制同装 `/usr/bin`）。

## 最近验收（2026-11-24，Deepin 25 / amd64）

- 1.0.0 Debian 主包构建通过：`dpkg-deb -I/-c`、conffiles、md5sums、依赖推导、
  `apt-get -s install` 模拟、临时 root 中 `dpkg --unpack` 均正常。
- 实机 `dpkg -i` 安装：启动菜单出现设置入口，daemon 可拉起，设置窗口正常工作。
- 打包过程中发现并修掉：启动菜单缺少主程序入口（补 `TrafficView` 桌面入口与会话启动器）、
  重复启动托盘/悬浮窗会产生第二份（补固定总线名）、D-Bus 激活文件干扰 daemon 集成测试
  （改为会话入口统一启动）。
- 同时修掉两处既有 clippy 报错（`ui-wayland` 的 `FrameSource` 大枚举、`ui-x11` 可合并的
  `match` 分支）。

## 最近验收（2026-11-25，1.0.2）

- 定位悬浮窗拖动不跟手：x11rb 将 `configure_window` 暂存在 16KB 发送缓冲，拖动期间只有
  下帧绘制才 flush；改为每批事件后主动 flush，拖动时轮询间隔 10ms→1ms。
- `draw` 在内容未变化的提前返回路径上也会 flush，位置预设和穿透切换不再等到下一帧。
- `fmt`、`clippy -D warnings`、`dbus-run-session -- cargo test --workspace` 通过；
  发行包 `dist/trafficview_1.0.2-1_amd64.deb` 构建成功。

## 最近验收（2026-11-24，1.0.1）

- 悬浮窗可读性：浅色背景下文字对比度不足，改为按背景色与整体透明度自动调整文字亮度，
  字号/字重/行高加大，并加 1px 内描边；`fmt`、`clippy -D warnings`、工作区测试通过。
- 托盘 `Network interface` 子菜单：定位到 `GetLayout` 忽略 `parent_id`，修复后子菜单
  返回 `Automatic` + 网卡单选项；补了布局单测。
- 托盘图标改为内嵌 64px 应用图标的静态图标，移除随速率重绘与 `NewIcon` 信号。
- `dist/trafficview_1.0.1-1_amd64.deb` 构建通过，`dpkg-deb -f` 校验版本与依赖正常；
  截图收录在 `docs/images/` 并写入 README 与 1.0.1 发行说明。

## 最近验收（2026-11-22，Cinnamon/X11）

- 桌面验收 15 项全过：托盘 6 项菜单、悬浮窗位置/透明度/穿透/皮肤热更新、设置窗口双向同步、断连降级与自动重连、开机自启文件写入。
- 过程中修掉 3 个真实缺陷（设置窗口保存必失败、空闲时感知不到 daemon 退出、无效皮肤路径杀死悬浮窗）。
- 托盘菜单点击无效的根因：Cinnamon 用 `EventGroup` 批量投递，托盘只实现了 `Event`。
- 退出残留悬浮窗的根因：daemon 退出后采样信号流不结束 + `try_recv` 把通道断开当成“暂时没帧”。

## 性能现状（M5 Task 2，`dist/` 产物，VirtualBox 7 vCPU）

| 指标 | 预算 | 实测 | 结论 |
| --- | --- | --- | --- |
| daemon 稳态 CPU（60s，1s 采样） | <0.3% | 0.400%（user 0.167 / sys 0.233） | **未达标** |
| daemon RSS | — | 10.6 MB | — |
| daemon + 悬浮窗 | <30 MB | 25.2 MB | 达标 |
| daemon + 悬浮窗 + 托盘 | <30 MB | 33.5 MB | 未达标 |
| `status` 延迟（30 次） | p95 <80ms | p95 54ms | 达标 |
| 皮肤预览（20 次） | p95 <150ms | p95 136ms | 达标 |
| 启动到 D-Bus 名可用 | <80ms | 21–34ms | 达标 |

基准（`cargo bench --workspace`）：格式化 0.75–0.95µs/6 值、单帧渲染 171µs、历史分钟查询 6.6µs / 日查询 5.4µs。

已做优化：0.617% → 0.400%（接口详情 netlink 事件驱动刷新 + 30s 兜底；历史库 `WAL + synchronous=NORMAL`）。5s 采样对照实测 0.133%，反推单样本约 0.7ms。

## 环境与磁盘

- 当前开发/打包环境：Deepin 25（crimson）/ amd64 / X11；GTK 4.18.6、libadwaita 1.5.0；
  Rust 1.100.0。
- 桌面验收环境另有 VirtualBox + Cinnamon/X11（无 GPU、单网卡）；`perf` 不可用
  （paranoid=4），profiling 用 strace 与 5s 采样对照。
- 磁盘：根分区 62G，已用 28G（约 47%），可正常执行 release 构建与测试。

## 提交记录（近期）

```
725aeb1 feat(overlay): 悬浮窗改 2×2 网格布局、自动贴靠屏幕、修复圆角
46cca14 docs: 增加项目 README 与运行说明
75bdff4 docs: 更新项目状态与明日续做入口
d22e84b test: 增加采集、渲染与存储基准及发布构建脚本
a925b56 feat: 托盘与 CLI 增加设置窗口入口
e7efef1 docs: 记录 M2 在 Cinnamon/X11 的桌面验收结果
```

1.0.1 与 1.0.2 的版本号、打包资源、悬浮窗与托盘修复、文档与截图改动尚未提交。

## 从这里继续

1. **重装并复核最新包**：`sudo dpkg -i dist/trafficview_1.0.2-1_amd64.deb`，确认启动菜单
   同时有 `TrafficView` 与 `TrafficView Settings`、重复点击不产生第二份托盘/悬浮窗、
   托盘子菜单与静态图标正常、悬浮窗拖动跟手、注销重登后自启动正常；随后复跑完整质量门。
2. **决定 CPU 预算方向**（阻塞 M5 Step 4）：继续优化慢变采集器/无订阅者跳过序列化，
   或在真实非虚拟化硬件复测后接受差距。
3. 提交 1.0.1/1.0.2 改动（版本、`packaging/`、单实例、悬浮窗与托盘修复、README、
   `docs/images/`、状态文档与发行说明）。
4. 继续 M5：Task 3 的 rpm/AppImage；Task 4 的 install/configuration/skins/plugins/
   desktop-support/permissions/release 专页；Task 1 的 TrafficMonitor 皮肤转换器。
5. **LICENSE 仍未提供**：打包测试可做，公开发布必须先由项目所有者决定许可证。

## 遗留清单（不阻塞发布）

- eBPF 内核侧（M4 Task 4 Step 3）：`process-ebpf` 为空 feature，`backend=ebpf` 恒返回不可用（已按规范降级并给原因）。
- M3 两处真机验收：网卡热插拔、重启/休眠/主题切换。
- M1/M2 计划复选框未勾：需要 XFCE/X11、KDE/Wayland、GNOME/Wayland、waybar 覆盖。
- M0–M2 三份计划的复选框仍为 0%，与代码实际交付不符，属记账欠账。
- 悬浮窗在 VirtualBox 软件渲染下会留旧像素残影（X 未重绘，非程序缺陷）。
- 包内未带 eBPF/AF_PACKET helper 与对应 system 单元；`backend=ebpf|af_packet` 恒降级。

## 时序语义（长期约束）

- 所有采样差分用 `CLOCK_MONOTONIC`；首帧、计数器倒退、`dt > 5s` 不生成速率。
- `ts_ms` 为自单调时钟零点起的毫秒数，跨进程同基准。
