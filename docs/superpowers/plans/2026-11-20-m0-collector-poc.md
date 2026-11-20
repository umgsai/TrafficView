# TrafficView M0 采集 PoC Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 建立 Rust workspace，并用可测试的 `/proc` 采集器输出网络速率、CPU 利用率和内存快照。

**Architecture:** `trafficview-core` 只定义采样模型和纯函数；`trafficview-collectors` 解析可注入的 procfs 路径并保存上一帧计数器；`trafficview sample` 负责单调时钟、采样间隔和 JSON 输出。首帧只建立基线，不输出伪造的零速率。

**Tech Stack:** Rust 2021、serde、serde_json、clap、anyhow、thiserror、tempfile（测试）。

**前置条件:** 无代码依赖。实时验收在 Linux/WSL2 中执行；Windows 只能运行不依赖真实 procfs 的纯解析测试。

---

## 文件结构

- Create: `Cargo.toml` — workspace 与共享编译配置。
- Create: `crates/core/Cargo.toml`
- Create: `crates/core/src/lib.rs` — `Snapshot`、`NetSample`、`CpuSample`、`MemSample`。
- Create: `crates/core/src/format.rs` — 1024 进制格式化和安全差分。
- Create: `crates/collectors/Cargo.toml`
- Create: `crates/collectors/src/lib.rs`
- Create: `crates/collectors/src/net.rs` — `/proc/net/dev`。
- Create: `crates/collectors/src/cpu.rs` — `/proc/stat`。
- Create: `crates/collectors/src/mem.rs` — `/proc/meminfo`。
- Create: `crates/cli/Cargo.toml`
- Create: `crates/cli/src/main.rs` — `trafficview sample`。
- Create: `crates/cli/tests/sample.rs` — CLI 输出契约。

### Task 1: 建立 workspace、采样模型与格式化纯函数

**Files:**
- Create: `Cargo.toml`
- Create: `crates/core/Cargo.toml`
- Create: `crates/core/src/lib.rs`
- Create: `crates/core/src/format.rs`

- [ ] **Step 1: 写入 workspace 与最小模型**

根 `Cargo.toml` 先只包含本阶段 crate，后续阶段再追加成员：

```toml
[workspace]
resolver = "2"
members = ["crates/core"]

[workspace.package]
version = "0.1.0"
edition = "2021"

[profile.release]
lto = "thin"
codegen-units = 1
strip = true
```

`crates/core/Cargo.toml`：

```toml
[package]
name = "trafficview-core"
version.workspace = true
edition.workspace = true

[dependencies]
serde = { version = "1", features = ["derive"] }
```

`crates/core/src/lib.rs`：

```rust
pub mod format;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetSample {
    pub iface: String,
    pub rx_bps: u64,
    pub tx_bps: u64,
    pub rx_total: u64,
    pub tx_total: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CpuSample {
    pub total_pct: f32,
    pub per_core: Vec<f32>,
    pub freq_mhz: Option<u32>,
    pub load: Option<[f32; 3]>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemSample {
    pub used_pct: f32,
    pub total: u64,
    pub available: u64,
    pub swap_pct: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub ts_ms: u64,
    pub net: NetSample,
    pub cpu: CpuSample,
    pub mem: MemSample,
}
```

- [ ] **Step 2: 先写格式化与差分失败测试**

在 `crates/core/src/format.rs` 先只写测试，不写实现：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn formats_binary_rates() {
        assert_eq!(format_rate(0), "0 B/s");
        assert_eq!(format_rate(1023), "1023 B/s");
        assert_eq!(format_rate(1024), "1.0 KB/s");
        assert_eq!(format_rate(1_572_864), "1.5 MB/s");
    }

    #[test]
    fn rejects_invalid_counter_deltas() {
        assert_eq!(counter_rate(100, 90, Duration::from_secs(1)), None);
        assert_eq!(counter_rate(100, 200, Duration::ZERO), None);
        assert_eq!(counter_rate(100, 200, Duration::from_secs(6)), None);
        assert_eq!(counter_rate(100, 350, Duration::from_millis(500)), Some(500));
    }
}
```

- [ ] **Step 3: 运行测试并确认编译失败**

Run: `cargo test -p trafficview-core`

Expected: FAIL，提示 `format_rate` 与 `counter_rate` 未定义；不要先写实现来规避这个失败。

- [ ] **Step 4: 写入最小实现并验证**

在测试模块前加入：

```rust
use std::time::Duration;

pub fn format_rate(bytes_per_sec: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes_per_sec < 1024 {
        return format!("{bytes_per_sec} B/s");
    }
    let mut value = bytes_per_sec as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}/s", UNITS[unit])
}

pub fn counter_rate(previous: u64, current: u64, elapsed: Duration) -> Option<u64> {
    if elapsed.is_zero() || elapsed > Duration::from_secs(5) || current < previous {
        return None;
    }
    Some(((current - previous) as f64 / elapsed.as_secs_f64()).round() as u64)
}
```

Run: `cargo test -p trafficview-core`

Expected: 2 passed, 0 failed。

- [ ] **Step 5: 提交独立变更**

```bash
git add Cargo.toml crates/core
git commit -m "feat: add core sampling models and rate formatting"
```

### Task 2: 实现网络解析、首帧基线和异常差分

**Files:**
- Modify: `Cargo.toml` — 将 members 改为 `["crates/core", "crates/collectors"]`
- Create: `crates/collectors/Cargo.toml`
- Create: `crates/collectors/src/lib.rs`
- Create: `crates/collectors/src/net.rs`

- [ ] **Step 1: 添加 crate 与失败测试**

`crates/collectors/Cargo.toml`：

```toml
[package]
name = "trafficview-collectors"
version.workspace = true
edition.workspace = true

[dependencies]
trafficview-core = { path = "../core" }
thiserror = "2"

[dev-dependencies]
tempfile = "3"
```

`crates/collectors/src/lib.rs`：

```rust
pub mod net;
```

`net.rs` 先加入测试：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "Inter-| Receive | Transmit\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\n eth0: 1000 1 0 0 0 0 0 0 2000 2 0 0 0 0 0 0\n";

    #[test]
    fn parses_selected_interface() {
        assert_eq!(parse_proc_net_dev(FIXTURE, "eth0"), Some((1000, 2000)));
        assert_eq!(parse_proc_net_dev(FIXTURE, "wlan0"), None);
    }

    #[test]
    fn first_sample_establishes_baseline() {
        let mut state = NetState::default();
        assert_eq!(state.update((100, 200), Duration::from_secs(1)), None);
        let second = state.update((1128, 2504), Duration::from_secs(1)).unwrap();
        assert_eq!((second.0, second.1), (1024, 2304));
    }

    #[test]
    fn reset_and_resume_only_rebuild_baseline() {
        let mut state = NetState::default();
        state.update((1000, 2000), Duration::from_secs(1));
        assert_eq!(state.update((10, 20), Duration::from_secs(1)), None);
        assert!(state.update((1034, 2100), Duration::from_secs(1)).is_some());
    }
}
```

- [ ] **Step 2: 确认测试失败**

Run: `cargo test -p trafficview-collectors net::`

Expected: FAIL，网络类型和函数尚未定义；workspace 此时只包含已创建的 core 与 collectors。

- [ ] **Step 3: 实现解析器与文件采集器**

在 `net.rs` 测试模块前加入：

```rust
use std::{fs, io, path::{Path, PathBuf}, time::Duration};
use trafficview_core::{format::counter_rate, NetSample};

pub fn parse_proc_net_dev(input: &str, iface: &str) -> Option<(u64, u64)> {
    input.lines().skip(2).find_map(|line| {
        let (name, values) = line.split_once(':')?;
        if name.trim() != iface { return None; }
        let fields: Vec<&str> = values.split_whitespace().collect();
        Some((fields.first()?.parse().ok()?, fields.get(8)?.parse().ok()?))
    })
}

#[derive(Default)]
pub struct NetState { previous: Option<(u64, u64)> }

impl NetState {
    pub fn update(&mut self, current: (u64, u64), elapsed: Duration) -> Option<(u64, u64)> {
        let previous = self.previous.replace(current);
        let (rx, tx) = previous?;
        Some((counter_rate(rx, current.0, elapsed)?, counter_rate(tx, current.1, elapsed)?))
    }
}

pub struct ProcNetDev { root: PathBuf, state: NetState }

impl ProcNetDev {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), state: NetState::default() }
    }

    pub fn sample(&mut self, iface: &str, elapsed: Duration) -> io::Result<Option<NetSample>> {
        let text = fs::read_to_string(self.root.join("net/dev"))?;
        let totals = parse_proc_net_dev(&text, iface)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("interface {iface} not found")))?;
        Ok(self.state.update(totals, elapsed).map(|(rx_bps, tx_bps)| NetSample {
            iface: iface.to_owned(), rx_bps, tx_bps, rx_total: totals.0, tx_total: totals.1,
        }))
    }
}

impl Default for ProcNetDev {
    fn default() -> Self { Self::new(Path::new("/proc")) }
}
```

- [ ] **Step 4: 运行网络测试与 lint**

Run: `cargo test -p trafficview-collectors net:: && cargo clippy -p trafficview-collectors --all-targets -- -D warnings`

Expected: 网络测试全部 PASS，clippy 无警告。

- [ ] **Step 5: 提交**

```bash
git add Cargo.toml crates/collectors/Cargo.toml crates/collectors/src/lib.rs crates/collectors/src/net.rs Cargo.lock
git commit -m "feat: sample Linux network counters safely"
```

### Task 3: 实现 CPU 与内存采集

**Files:**
- Modify: `crates/collectors/src/lib.rs` — 增加 `pub mod cpu; pub mod mem;`
- Create: `crates/collectors/src/cpu.rs`
- Create: `crates/collectors/src/mem.rs`

- [ ] **Step 1: 写 CPU 差分和内存口径失败测试**

先在 `lib.rs` 导出两个新模块，再写下列测试，使失败原因保持在未定义函数而不是缺失模块。

`cpu.rs` 测试：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_total_and_per_core_usage() {
        let before = parse_proc_stat("cpu 100 0 100 800 0 0 0 0 0 0\ncpu0 100 0 100 800 0 0 0 0 0 0\n").unwrap();
        let after = parse_proc_stat("cpu 150 0 150 1100 0 0 0 0 0 0\ncpu0 150 0 150 1100 0 0 0 0 0 0\n").unwrap();
        let (total, cores) = cpu_usage(&before, &after).unwrap();
        assert_eq!(total, 25.0);
        assert_eq!(cores, vec![25.0]);
    }

    #[test]
    fn rejects_boot_counter_reset() {
        let before = parse_proc_stat("cpu 100 0 100 800 0 0 0 0 0 0\n").unwrap();
        let after = parse_proc_stat("cpu 1 0 1 8 0 0 0 0 0 0\n").unwrap();
        assert_eq!(cpu_usage(&before, &after), None);
    }
}
```

`mem.rs` 测试：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_mem_available_and_handles_no_swap() {
        let input = "MemTotal: 1000 kB\nMemAvailable: 250 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n";
        let sample = parse_meminfo(input).unwrap();
        assert_eq!(sample.total, 1_024_000);
        assert_eq!(sample.available, 256_000);
        assert_eq!(sample.used_pct, 75.0);
        assert_eq!(sample.swap_pct, 0.0);
    }
}
```

- [ ] **Step 2: 确认失败**

Run: `cargo test -p trafficview-collectors cpu:: && cargo test -p trafficview-collectors mem::`

Expected: FAIL，解析与计算函数尚未定义。

- [ ] **Step 3: 实现 CPU 解析与差分**

在 `cpu.rs` 测试前加入：

```rust
use std::{fs, io, path::{Path, PathBuf}};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuTimes { total: u64, busy: u64 }

pub fn parse_proc_stat(input: &str) -> Option<Vec<CpuTimes>> {
    let rows: Vec<CpuTimes> = input.lines().filter(|line| line.starts_with("cpu")).filter_map(|line| {
        let values: Option<Vec<u64>> = line.split_whitespace().skip(1).take(8).map(str::parse).map(Result::ok).collect();
        let values = values?;
        if values.len() < 8 { return None; }
        let total = values.iter().sum();
        let busy = total - values[3] - values[4];
        Some(CpuTimes { total, busy })
    }).collect();
    (!rows.is_empty()).then_some(rows)
}

pub fn cpu_usage(before: &[CpuTimes], after: &[CpuTimes]) -> Option<(f32, Vec<f32>)> {
    if before.len() != after.len() { return None; }
    let usage: Option<Vec<f32>> = before.iter().zip(after).map(|(a, b)| {
        let total = b.total.checked_sub(a.total)?;
        let busy = b.busy.checked_sub(a.busy)?;
        (total > 0).then_some(100.0 * busy as f32 / total as f32)
    }).collect();
    let usage = usage?;
    Some((usage[0], usage[1..].to_vec()))
}

pub struct ProcCpu { root: PathBuf, previous: Option<Vec<CpuTimes>> }

impl ProcCpu {
    pub fn new(root: impl Into<PathBuf>) -> Self { Self { root: root.into(), previous: None } }
    pub fn sample(&mut self) -> io::Result<Option<(f32, Vec<f32>)>> {
        let current = parse_proc_stat(&fs::read_to_string(self.root.join("stat"))?)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid /proc/stat"))?;
        let previous = self.previous.replace(current.clone());
        Ok(previous.and_then(|old| cpu_usage(&old, &current)))
    }
}

impl Default for ProcCpu {
    fn default() -> Self { Self::new(Path::new("/proc")) }
}
```

- [ ] **Step 4: 实现内存采集**

在 `mem.rs` 测试前加入：

```rust
use std::{collections::HashMap, fs, io, path::Path};
use trafficview_core::MemSample;

pub fn parse_meminfo(input: &str) -> Option<MemSample> {
    let values: HashMap<&str, u64> = input.lines().filter_map(|line| {
        let (key, rest) = line.split_once(':')?;
        Some((key, rest.split_whitespace().next()?.parse::<u64>().ok()? * 1024))
    }).collect();
    let total = *values.get("MemTotal")?;
    let available = *values.get("MemAvailable")?;
    let swap_total = values.get("SwapTotal").copied().unwrap_or(0);
    let swap_free = values.get("SwapFree").copied().unwrap_or(0);
    if total == 0 || available > total || swap_free > swap_total { return None; }
    Some(MemSample {
        used_pct: 100.0 * (total - available) as f32 / total as f32,
        total,
        available,
        swap_pct: if swap_total == 0 { 0.0 } else { 100.0 * (swap_total - swap_free) as f32 / swap_total as f32 },
    })
}

pub fn read_mem(root: &Path) -> io::Result<MemSample> {
    let text = fs::read_to_string(root.join("meminfo"))?;
    parse_meminfo(&text).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid /proc/meminfo"))
}
```

- [ ] **Step 5: 验证并提交**

Run: `cargo test -p trafficview-collectors && cargo clippy -p trafficview-collectors --all-targets -- -D warnings`

Expected: 全部 PASS，clippy 无警告。

```bash
git add crates/collectors/src/lib.rs crates/collectors/src/cpu.rs crates/collectors/src/mem.rs
git commit -m "feat: collect CPU and memory samples from procfs"
```

### Task 4: 交付可脚本消费的 `trafficview sample`

**Files:**
- Modify: `Cargo.toml` — 将 `crates/cli` 加入 members
- Create: `crates/cli/Cargo.toml`
- Create: `crates/cli/src/main.rs`
- Create: `crates/cli/tests/sample.rs`

- [ ] **Step 1: 写 CLI 帮助契约失败测试**

`crates/cli/Cargo.toml`：

```toml
[package]
name = "trafficview-cli"
version.workspace = true
edition.workspace = true

[[bin]]
name = "trafficview"
path = "src/main.rs"

[dependencies]
anyhow = "1"
clap = { version = "4", features = ["derive"] }
serde_json = "1"
trafficview-core = { path = "../core" }
trafficview-collectors = { path = "../collectors" }

[dev-dependencies]
assert_cmd = "2"
predicates = "3"
```

`crates/cli/tests/sample.rs`：

```rust
use assert_cmd::Command;
use predicates::str::contains;

#[test]
fn sample_requires_an_explicit_interface_in_m0() {
    Command::cargo_bin("trafficview").unwrap()
        .args(["sample", "--help"])
        .assert().success()
        .stdout(contains("--interface <INTERFACE>"))
        .stdout(contains("--interval-ms <INTERVAL_MS>"))
        .stdout(contains("--count <COUNT>"));
}
```

- [ ] **Step 2: 确认测试失败**

Run: `cargo test -p trafficview-cli --test sample`

Expected: FAIL，`trafficview` 二进制尚不存在。

- [ ] **Step 3: 实现最小 CLI**

`crates/cli/src/main.rs`：

```rust
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::{path::Path, thread, time::{Duration, Instant}};
use trafficview_collectors::{cpu::ProcCpu, mem::read_mem, net::ProcNetDev};
use trafficview_core::{CpuSample, Snapshot};

#[derive(Parser)]
#[command(name = "trafficview", version)]
struct Cli { #[command(subcommand)] command: Commands }

#[derive(Subcommand)]
enum Commands {
    Sample {
        #[arg(long)] interface: String,
        #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(500..=30_000))] interval_ms: u64,
        #[arg(long, default_value_t = 1)] count: usize,
    },
}

fn main() -> Result<()> {
    let Cli { command } = Cli::parse();
    match command {
        Commands::Sample { interface, interval_ms, count } => {
            if count == 0 { bail!("--count must be greater than zero"); }
            let started = Instant::now();
            let mut previous = started;
            let mut net = ProcNetDev::default();
            let mut cpu = ProcCpu::default();
            let _ = net.sample(&interface, Duration::ZERO).context("establish network baseline")?;
            let _ = cpu.sample().context("establish CPU baseline")?;
            for _ in 0..count {
                thread::sleep(Duration::from_millis(interval_ms));
                let now = Instant::now();
                let elapsed = now.duration_since(previous);
                previous = now;
                let net = net.sample(&interface, elapsed)?.context("network baseline was invalidated; retry")?;
                let (total_pct, per_core) = cpu.sample()?.context("CPU baseline was invalidated; retry")?;
                let snapshot = Snapshot {
                    ts_ms: started.elapsed().as_millis() as u64,
                    net,
                    cpu: CpuSample { total_pct, per_core, freq_mhz: None, load: None },
                    mem: read_mem(Path::new("/proc"))?,
                };
                println!("{}", serde_json::to_string(&snapshot)?);
            }
            Ok(())
        }
    }
}
```

- [ ] **Step 4: 运行自动测试和真实输出检查**

Run: `cargo test -p trafficview-cli && cargo run -p trafficview-cli -- sample --interface eth0 --count 2`

Expected: 自动测试 PASS；若 Linux 上网卡不是 `eth0`，先用 `ip -brief link` 选择真实名称。输出两行独立 JSON，包含 `ts_ms/net/cpu/mem`，没有首帧额外输出。

- [ ] **Step 5: 提交**

```bash
git add Cargo.toml crates/cli Cargo.lock
git commit -m "feat: expose collector PoC through sample CLI"
```

### Task 5: 完成 M0 校准与质量门

**Files:**
- Modify: `crates/collectors/src/net.rs` — 仅在实测发现解析或计时问题时修正，并先补回归测试。
- Modify: `crates/collectors/src/cpu.rs` — 仅在实测发现口径问题时修正，并先补回归测试。

- [ ] **Step 1: 运行完整静态检查**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: 三条命令退出码均为 0。

- [ ] **Step 2: 与系统工具同步校准**

在 Linux 上先执行 `ip route show default` 确认网卡，再启动持续但稳定的网络流量，同时运行：

```bash
IFACE=wlan0  # 示例；先赋值为 ip route 显示的真实网卡
cargo run --release -p trafficview-cli -- sample --interface "$IFACE" --count 20 --interval-ms 1000
nload "$IFACE"
htop
```

Expected: 对齐同一秒窗口后，稳态上下行速率及总 CPU 与对照工具误差 < 3%；不同工具刷新时刻未对齐的样本必须重测，不能修改数据来凑结果。

- [ ] **Step 3: 验证异常时序**

挂起并恢复 Linux 虚拟机，或注入 `Duration::from_secs(6)` 的单元测试帧。

Expected: `dt > 5s` 的帧被丢弃，下一帧重新建立基线；没有负值、瞬时速率尖峰或 panic。

- [ ] **Step 4: 记录最终验证并提交修正**

若没有代码修正，不创建空提交；若有修正：

```bash
git add crates/collectors/src/net.rs crates/collectors/src/cpu.rs
git commit -m "fix: align collector deltas with Linux tools"
```

M0 完成条件：上述命令均有真实 PASS 输出，CLI 能持续输出合法 JSON，且校准达到 < 3% 后才可执行 M1。
