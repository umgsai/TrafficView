# TrafficView 开发总路线图

> **For agentic workers:** 按顺序执行下列子计划。每份子计划必须使用 `superpowers:subagent-driven-development`（推荐）或 `superpowers:executing-plans` 逐项实施，步骤使用复选框跟踪。

**Goal:** 将 `docs/linux-technical-plan.md` 的调研结论拆成六个可独立验收、按依赖顺序交付的开发阶段。

**Architecture:** Rust workspace 中的 `trafficviewd` 统一采集并通过 session D-Bus 发布快照；`trafficview` 提供 CLI、托盘和悬浮窗；GNOME、Plasma、waybar 等前端只消费同一份数据。核心模型、采集、渲染和平台窗口分别放在独立 crate 中。

**Tech Stack:** Rust 2021、Tokio、serde/TOML、zbus、x11rb、wayland-client、ksni、tiny-skia、cosmic-text、rusqlite、GJS/QML、aya（可选）。

---

## 1. 计划依据与默认决策

唯一需求基线是 [linux-technical-plan.md](../../linux-technical-plan.md)。当前仓库没有实现代码，因此从 workspace 与数据模型开始，不做 Windows 代码移植。

采用调研文档的推荐范围，不等待额外产品决策：

- TrafficMonitor 皮肤转换器进入 M5；M2 先实现原生 `skin.toml`。
- GNOME 45+ 是一等桌面，M2 交付 Shell 扩展；GNOME/Wayland 悬浮窗仅承诺普通窗口降级。
- 进程级流量进入 M4，默认关闭；无能力时必须明确降级，不阻塞基础监控。
- 不实现 Web/HTTP/远程查看。
- 主分发渠道为 deb/rpm/AppImage；Flatpak/Snap 不作为发布门槛。
- `trafficview tray|overlay|status|config|skin` 共用一个 CLI 二进制，后台为独立 `trafficviewd`。

## 2. 子计划与交付物

| 顺序 | 计划 | 可独立验收的产物 | 前置 |
|---|---|---|---|
| 1 | [M0：采集 PoC](./2026-11-20-m0-collector-poc.md) | `trafficview sample` 输出网络、CPU、内存 JSON；计数器重置与休眠测试通过 | 无 |
| 2 | [M1：Linux X11 MVP](./2026-11-20-m1-x11-mvp.md) | daemon + D-Bus + X11 悬浮窗 + SNI 托盘 + TOML + 自启动 | M0 验收 |
| 3 | [M2：跨桌面展示与皮肤](./2026-11-20-m2-display-and-skins.md) | 原生皮肤、Wayland layer-shell、GNOME 扩展、waybar 输出 | M1 验收 |
| 4 | [M3：历史、告警与网卡策略](./2026-11-20-m3-history-alerts-network.md) | SQLite 日月统计、告警通知、自动选卡、深浅色切换 | M2 验收 |
| 5 | [M4：硬件、插件与进程流量](./2026-11-20-m4-advanced-metrics.md) | 磁盘/GPU/温度、子进程插件、可选 eBPF、设置界面 | M3 验收 |
| 6 | [M5：发布与兼容工具](./2026-11-20-m5-release.md) | deb/rpm/AppImage、皮肤转换器、基准与发布文档 | M4 验收 |

## 3. 依赖与执行规则

```text
M0 采集语义
  └─ M1 D-Bus 契约 + X11 可用产品
       └─ M2 展示契约 + 桌面适配
            └─ M3 持久化与告警
                 └─ M4 可选高级能力
                      └─ M5 发布
```

- 默认严格按顺序执行；只有文档撰写或不修改同一 crate 的独立验证工作可以并行。
- 每个阶段先运行前一阶段的全部回归测试，再勾选新任务。
- D-Bus 接口在 M1 冻结 v1；M2–M4 只能增加可选字段或新接口，不能改变已有字段含义。
- 所有采样差分使用 `CLOCK_MONOTONIC`；首帧、计数器倒退和 `dt > 5s` 不生成速率。
- 所有前端均不得直接读取 `/proc`、`/sys` 或历史数据库。
- 可选能力缺失时返回明确的 `available=false`/空值与原因，不伪造零值。

## 4. 统一文件边界

```text
Cargo.toml
crates/
  core/          # Snapshot、DisplayItem、格式化、共享错误与配置类型
  collectors/    # Linux 指标采集；不包含 UI、D-Bus 或存储
  daemon/        # trafficviewd、调度、D-Bus、SQLite、告警、插件宿主
  cli/           # trafficview 二进制及 status/config/sample/skin 子命令
  render/        # 皮肤解析与平台无关 ARGB 渲染
  ui-x11/        # X11 窗口、RandR、输入区域
  ui-wayland/    # layer-shell、fractional-scale、输入区域
  tray/          # StatusNotifierItem
  plugin-proto/  # trafficview-plugin-v1 行分隔 JSON 类型
frontends/
  gnome-extension/
  plasma-widget/
packaging/
  systemd/ deb/ rpm/ appimage/
```

同一文件不跨阶段重复定义职责；需要扩展时由对应阶段修改。新增公共字段先更新 `core` 的序列化往返测试，再修改消费者。

## 5. 每阶段强制质量门

以下命令在 Linux 或 WSL2 中运行；X11/Wayland/GNOME 验收必须在对应真实图形会话中完成，不能以交叉编译代替。

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
```

M4 的 eBPF feature 使用独立命令验证，不能让无 eBPF 工具链的环境无法构建默认 workspace：

```bash
cargo check -p trafficview-daemon --features process-ebpf
cargo test -p trafficview-core --all-features
```

阶段完成还必须满足：

- 所有新增测试先失败、后通过，并保留命令输出作为执行记录。
- 配置、D-Bus、插件和数据库格式均有序列化/迁移测试。
- 崩溃、权限不足、会话不支持时没有未处理 panic。
- 用户可见行为与调研中的支持矩阵一致，尤其不宣称 GNOME 支持 layer-shell。

## 6. 总体验收矩阵

| 能力 | 最终验收环境 | 通过条件 |
|---|---|---|
| 采集正确性 | Linux，对照 nload/htop | 稳态速率与 CPU 误差 < 3%，首帧无尖峰 |
| X11 MVP | XFCE 4.18/X11 | 托盘、悬浮窗、配置热加载可用，连续 24h 无持续 RSS 增长 |
| KDE Wayland | Plasma 6 | layer-shell、HiDPI、鼠标穿透与托盘可用 |
| GNOME 45+ | Wayland | Shell 扩展显示顶栏文字；普通窗口降级说明准确 |
| waybar | Sway 或 Hyprland | `--format waybar` 是合法单行 JSON 并可轮询 |
| 数据连续性 | 重启/休眠/网卡重置用例 | 不重复累计，不产生负速率或异常尖峰 |
| 权限降级 | 无 capabilities、无 NVML、插件崩溃 | 基础指标持续更新，原因可诊断 |
| 发布 | Ubuntu、Fedora、Arch 目标环境 | deb/rpm/AppImage 安装后无需 GTK/Qt 即可运行 |

## 7. 完成定义

只有子计划全部勾选、质量门通过、该阶段验收命令有真实输出，才能进入下一阶段。执行结束后的合并、PR 或分支清理由 `superpowers:finishing-a-development-branch` 处理，不在这些计划中隐式推送远端。
