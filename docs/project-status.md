# TrafficView 项目状态

> 最后更新：2026-11-22
> 当前里程碑：M0–M4 已完成（M2/M3 有少量真机项未覆盖），M5 进行中（Task 2 已完成，但性能预算未达标）

## 一句话

采集、D-Bus 守护进程、X11 悬浮窗、托盘、GTK4 设置界面与历史/告警均已实现并在 Cinnamon/X11 实机跑通；发布工程（基准、构建脚本）已就绪，卡在 daemon 稳态 CPU 0.400% 未达 0.3% 预算。

## 里程碑状态

| 里程碑 | 状态 | 说明 |
| --- | --- | --- |
| M0 采集 PoC | 完成 | `/proc` 采集、1024 进制格式化、单调时钟差分 |
| M1 X11 MVP | 代码完成 | 悬浮窗 + 托盘 + D-Bus v1 契约冻结；计划复选框未勾（差 Wayland/其它桌面） |
| M2 显示与皮肤 | 代码完成 | Cinnamon/X11 15 项验收通过；KDE/GNOME/Wayland/waybar 未覆盖，完成条件未满足 |
| M3 历史/告警/网络 | 基本完成 | 两处真机项未验：网卡热插拔、重启/休眠/主题切换 |
| M4 高级指标 | 完成 | GPU/eBPF 无硬件实测；`process-ebpf` 为空占位 feature，`backend=ebpf` 恒降级 |
| M5 发布工程 | 进行中 | Task 2 完成（Step 4 未达标未勾）；Task 1/3/4 未开始 |

## 代码结构

| 路径 | 作用 |
| --- | --- |
| `crates/core` | 模型、配置、格式化、时钟 |
| `crates/collectors` | `/proc`、sysfs、netlink、eBPF/AF_PACKET helper |
| `crates/daemon` | D-Bus 服务 `com.trafficview.Daemon1`、历史 SQLite、告警、主题 |
| `crates/cli` | `trafficview` 命令行（status/history/config/skin/overlay/tray/settings/autostart） |
| `crates/tray` | StatusNotifierItem + DBusMenu |
| `crates/ui-x11` | X11 悬浮窗（tiny-skia 软件渲染） |
| `crates/render` | 皮肤解析与离屏渲染 |
| `frontends/settings` | 独立 `[workspace]` 的 GTK4/libadwaita 设置界面（根 `Cargo.toml` 用 `exclude` 排除，根构建不解析 GTK） |
| `packaging/` | systemd 单元、D-Bus 策略、`build-release.sh` |

## 怎么跑

```bash
# 构建
cargo build --workspace --release                 # CLI + daemon
cargo build --manifest-path frontends/settings/Cargo.toml   # 设置界面（独立 workspace）

# 运行（需同一会话总线，DISPLAY=:0）
./target/release/trafficviewd &
./target/release/trafficview overlay &
./target/release/trafficview tray &
./target/release/trafficview settings            # 或托盘右键 → Settings…
```

设置界面查找顺序：与 `trafficview` 同目录 → `PATH`。本地调试需要软链：
`ln -sf ../../frontends/settings/target/debug/trafficview-settings target/release/trafficview-settings`（`target/` 不入库，打包时两二进制同装 `/usr/bin`）。

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

- GTK4 4.14.5 / libadwaita 1.5.0（`libgtk-4-dev`、`libadwaita-1-dev` 已装）；Rust 1.100.0（`PATH` 需含 `/home/shang/.cargo/bin`）。
- **磁盘极紧：根分区 24G，仅剩约 350MB。** `cargo clean --profile dev` 可释放约 3.5G；`cargo test` 会重建 debug 产物，跑前先确认空间。
- 无 GPU（VirtualBox），无 eBPF 测试条件；`perf` 不可用（paranoid=4），profiling 用 strace 与 5s 采样对照。

## 提交记录（近期，远端已同步到 `a925b56`）

```
d22e84b test: 增加采集、渲染与存储基准及发布构建脚本   ← 未推送
a925b56 feat: 托盘与 CLI 增加设置窗口入口
e7efef1 docs: 记录 M2 在 Cinnamon/X11 的桌面验收结果
1bc3b77 fix: 修复设置窗口保存失败与断连无感知
c47230d fix: daemon 退出后关闭残留的悬浮窗
97ac6df fix: 修复托盘菜单点击无效与悬浮窗不响应配置
```

## 明天从这里继续

1. **先决定 CPU 预算方向**（唯一阻塞 M5 Step 4 的事）：
   - 继续优化：慢变采集器（温度/GPU/文件系统/电池）降为 5s、无订阅者时跳过 `Sample` 序列化；
   - 或接受差距，在真实非虚拟化硬件复测后确认。
2. 若预算问题解决，勾选 M5 Task 2 Step 4 并 `git push origin master`（当前 1 个未推送提交）。
3. 进入 **M5 Task 3**（deb/rpm/AppImage 打包，见 `docs/superpowers/plans/2026-11-20-m5-release.md`）；之后 Task 4（文档 + CHANGELOG + 最终验收）、Task 1（TrafficMonitor 皮肤转换器）。
4. **LICENSE 仍未提供**：打包测试可做，公开发布必须先由项目所有者决定许可证。

## 遗留清单（不阻塞 M5）

- eBPF 内核侧（M4 Task 4 Step 3）：`process-ebpf` 为空 feature，`backend=ebpf` 恒返回不可用（已按规范降级并给原因）。
- M3 两处真机验收：网卡热插拔、重启/休眠/主题切换。
- M1/M2 计划复选框未勾：需要 XFCE/X11、KDE/Wayland、GNOME/Wayland、waybar 覆盖。
- M0–M2 三份计划的复选框仍为 0%，与代码实际交付不符，属记账欠账。
- 悬浮窗在 VirtualBox 软件渲染下会留旧像素残影（X 未重绘，非程序缺陷）。

## 时序语义（长期约束）

- 所有采样差分用 `CLOCK_MONOTONIC`；首帧、计数器倒退、`dt > 5s` 不生成速率。
- `ts_ms` 为自单调时钟零点起的毫秒数，跨进程同基准。
