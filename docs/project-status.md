# TrafficView 项目状态

> 最后更新：2026-11-20
> 当前里程碑：M0 已完成，M1 尚未开始

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
4. CPU 差分没有 dt 门限（计划给的 `ProcCpu` API 不含时间戳），建议 M1 daemon 调度在 `dt>5s` 时跳帧。
5. `ts_ms` 是相对进程启动的单调毫秒，不是 CLOCK_MONOTONIC 绝对时间。

## 下次开工前需要拍板

1. `ts_ms` 的语义（相对启动 vs 绝对单调时钟）——M1 冻结 D-Bus 契约前必须定。
2. CPU 差分是否在 M1 的 daemon 层加 `dt>5s` 跳帧。

## superpowers 安装状态

- 装在项目内 `/home/shang/workspace/TrafficView/.trae/skills/`（15 个 skill，v7.0.2，与源逐字节一致），`.trae/` 已被 `.gitignore` 忽略。
- TRAE 无官方支持、无 SessionStart hook，不会自动注入 `using-superpowers`；靠 description 触发或显式点名（如 `brainstorming`、`writing-plans`）。
- skill 列表在会话启动时固定，**新装后需新开会话才生效**。
- 全局安装曾被沙箱拦截，`~/.trae-cn/skills` 留着空目录，可忽略。

## 下一步

M1（X11 MVP）：`docs/superpowers/plans/2026-11-20-m1-x11-mvp.md`。开工前先定上面两个问题。
