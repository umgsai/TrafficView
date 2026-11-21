# TrafficView 项目状态

> 最后更新：2026-11-21
> 当前里程碑：M0 已完成（含时序语义对齐），M1 尚未开始

## 一句话

M0 的采集 PoC 已在本机真实跑通并通过校准，可执行产物在 `target/release/trafficview`。

## 已完成（M0）

- Rust workspace 三 crate：
  - `crates/core`：`Snapshot` / `NetSample` / `CpuSample` / `MemSample`，1024 进制格式化与安全差分
  - `crates/collectors`：`/proc/net/dev`、`/proc/stat`、`/proc/meminfo`，首帧只建基线
  - `crates/cli`：`trafficview sample`，单调时钟间隔，逐行 JSON
- 质量门全绿：`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`（10 个测试）、`cargo build --workspace --release`
- 真实采集：本机网卡是 `enp0s3`（无 eth0），首帧不外泄，采样间隔实测 1000–1009 ms
- 错误路径：未知网卡、`--count 0`、非法 interval 均被拒绝且无 panic
- 校准（3 MB/s 持续下行，20×1s 同窗口）：rx 0.43%、tx 0.76%、CPU(vmstat) 0.14%、CPU(htop) 0.40%、内存 used% 0.01%
- 异常时序：注入 `dt=6s` 的帧被丢弃、下一帧重建基线（见 `crates/collectors/src/net.rs`）；计数倒退同样只重建基线

### 怎么跑

```bash
cargo build --workspace --release
./target/release/trafficview sample --interface enp0s3 --count 3
```

只读 `/proc`，不需要 sudo。产物是 x86-64 动态链接 glibc 的 ELF，换发行版或架构需重新编译。

## 环境改动（均为用户级，无 sudo）

- Rust stable 装在 `~/.rustup` + `~/.cargo`，用清华 TUNA 镜像下载；未改系统包、未加全局 PATH
- 仅为本仓库设置 git 作者 `Shang <umgsai@126.com>`
- 用户手动安装了 `htop`（用于 CPU 对照）

## 提交记录

```
7ceeea3 test: drop long-gap frame and rebuild network baseline
49bb959 feat: expose collector PoC through sample CLI
5583e90 feat: collect CPU and memory samples from procfs
565d22e feat: add core sampling models and rate formatting
37fb4f8 增加开发计划   ← 原有历史未动
```

## 计划与现实的偏差（未改需求）

1. M0 计划 Task 2 断言算错：`1128−100` 应为 1028，计划写 1024；代码按正确值。
2. `thiserror` 未使用会触发 unused-dependency，与「clippy 无警告」冲突，已从 collectors 依赖移除，M1 需要错误类型时再加。
3. 计划要求用 nload 对照，但安装需 sudo；改用 `ip -s link`。htop 由用户安装后纳入 CPU 对照。
4. ~~CPU 差分没有 dt 门限~~ → 已修（`813e2aa`）：`dt==0` 或 `dt>5s` 只更新基线不出值，与网络采集一致。
5. ~~`ts_ms` 是相对进程启动的毫秒~~ → 已修（`813e2aa`）：改为 CLOCK_MONOTONIC 绝对基准毫秒（`crates/core/src/clock.rs`）。

## 时序语义（已对齐路线图第 3 节）

- 所有采样差分使用 `CLOCK_MONOTONIC`；首帧、计数器倒退、`dt > 5s` 不生成速率。
- `ts_ms` = 自 CLOCK_MONOTONIC 零点（通常为开机，不含休眠）起的毫秒数，跨进程同一基准。

## 已知但暂不处理

- 本仓库尚未 push 到 GitHub：本机无任何 GitHub 凭据（无 credential helper、无 SSH key、无 gh）。本地领先远端 5+ 个提交，等你提供 PAT 或 SSH key 后 `git push origin master`。

## superpowers 安装状态

- 装在项目内 `/home/shang/workspace/TrafficView/.trae/skills/`（15 个 skill，v7.0.2，与源逐字节一致），`.trae/` 已被 `.gitignore` 忽略。
- TRAE 无官方支持、无 SessionStart hook，不会自动注入 `using-superpowers`；靠 description 触发或显式点名（如 `brainstorming`、`writing-plans`）。
- skill 列表在会话启动时固定，**新装后需新开会话才生效**。
- 全局安装曾被沙箱拦截，`~/.trae-cn/skills` 留着空目录，可忽略。

## 下一步

M1（X11 MVP）：`docs/superpowers/plans/2026-11-20-m1-x11-mvp.md`。M0 的前置问题已清零，可直接开工；D-Bus v1 契约在 M1 冻结。
