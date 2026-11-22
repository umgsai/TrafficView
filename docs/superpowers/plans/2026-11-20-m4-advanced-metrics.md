# TrafficView M4 硬件、插件与进程流量 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 交付磁盘、CPU 频率/负载、温度、GPU、电池、子进程插件、可选进程级流量和独立 GTK4 设置界面。

**Architecture:** 新采集器都实现统一的可用性报告，缺失设备或权限时返回 `None`/不可用原因。插件以行分隔 JSON 子进程运行；进程流量由可选特权 helper 通过 system D-Bus 提供，user daemon 只读取聚合数据。GTK 设置是独立 Cargo 项目和进程，不进入主 workspace 的默认依赖。

**Tech Stack:** M3 + rtnetlink/procfs/sysfs、nvml-wrapper、std::process + serde_json、aya（可选）、gtk4/libadwaita（仅 settings）、zbus system bus。

**前置条件:** M3 全部通过。默认构建仍无需 GTK、eBPF 工具链或 root；eBPF 在支持 `CAP_BPF/CAP_PERFMON` 的 Linux 测试机或 VM 验收，禁止用 CI 容器特权缺失代替降级测试。

---

## 文件结构

- Create: `crates/collectors/src/disk.rs` — 整盘速率和 statvfs。
- Create: `crates/collectors/src/hwmon.rs` — CPU/GPU 温度。
- Create: `crates/collectors/src/gpu.rs` — AMD/NVIDIA/Intel GPU。
- Create: `crates/collectors/src/power.rs` — 电池。
- Create: `crates/collectors/src/uptime.rs` — `/proc/uptime`。
- Modify: `crates/collectors/src/cpu.rs` — cpufreq 与 loadavg。
- Modify: `crates/core/src/lib.rs` — 新快照类型与可用性原因。
- Create: `crates/plugin-proto/Cargo.toml`
- Create: `crates/plugin-proto/src/lib.rs` — v1 协议类型与帧校验。
- Create: `crates/daemon/src/plugins.rs` — 发现、调度、超时、退避。
- Create: `crates/daemon/tests/plugins.rs`
- Create: `crates/daemon/src/process_traffic.rs` — system D-Bus consumer 与降级。
- Create: `crates/daemon/src/bin/trafficviewd-ebpf.rs` — 可选特权 helper。
- Create: `crates/netprobe-ebpf/Cargo.toml` — 独立 no_std eBPF crate，不加入默认 members。
- Create: `crates/netprobe-ebpf/src/lib.rs` — 内核侧 map 与 probes。
- Modify: `crates/daemon/Cargo.toml` — `process-ebpf` feature。
- Create: `packaging/systemd/trafficviewd-ebpf.service`
- Create: `packaging/systemd/trafficviewd-afpacket.service`
- Create: `packaging/dbus/com.trafficview.Ebpf1.conf`
- Create: `frontends/settings/Cargo.toml` — 独立 workspace。
- Create: `frontends/settings/src/main.rs`
- Create: `frontends/settings/src/window.rs`
- Create: `frontends/settings/src/dbus.rs`

### Task 1: 增加磁盘、CPU 频率/负载、温度与电池采集

**Files:**
- Create: `crates/collectors/src/disk.rs`
- Create: `crates/collectors/src/hwmon.rs`
- Create: `crates/collectors/src/power.rs`
- Create: `crates/collectors/src/uptime.rs`
- Modify: `crates/collectors/src/lib.rs` — 导出 disk/hwmon/power/uptime
- Modify: `crates/collectors/src/cpu.rs`
- Modify: `crates/core/src/lib.rs`
- Modify: `crates/daemon/src/service.rs`
- Modify: `crates/daemon/src/dbus.rs`

- [x] **Step 1: 写解析、差分和设备过滤失败测试**

磁盘 fixture 验证扇区×512、首帧 None、计数器 reset None、`dt>5s` None，过滤 `loop/ram/sr` 和分区，只聚合整盘。uptime fixture 验证 `/proc/uptime` 第一个浮点字段转换为非负 `uptime_s`，倒退或非法值返回 None。CPU fixture 验证 kHz→MHz、部分 cpufreq 文件不可读时返回可用核平均、`loadavg` 三个浮点数。hwmon fixture 验证 `coretemp/k10temp` 名称、label、毫摄氏度转换、非有限值和超出 -50–150°C 的读数被拒绝。电池 fixture 验证多 BAT 按容量加权、无电池返回 None。

```rust
#[test]
fn diskstats_uses_512_byte_sectors_and_whole_disks() {
    let before = parse_diskstats(" 8 0 sda 1 0 100 0 1 0 200 0 0 0 0\n 8 1 sda1 1 0 50 0 1 0 50 0 0 0 0\n 7 0 loop0 1 0 999 0 0 0 0 0 0 0 0\n").unwrap();
    let after = parse_diskstats(" 8 0 sda 1 0 202 0 1 0 404 0 0 0 0\n").unwrap();
    let sample = disk_delta(&before[0], &after[0], Duration::from_secs(1)).unwrap();
    assert_eq!(sample.read_bps, 102 * 512);
    assert_eq!(sample.write_bps, 204 * 512);
}
```

- [x] **Step 2: 确认失败**

Run: `cargo test -p trafficview-collectors disk:: && cargo test -p trafficview-collectors hwmon:: && cargo test -p trafficview-collectors power:: && cargo test -p trafficview-collectors uptime:: && cargo test -p trafficview-collectors cpu::`

Expected: FAIL，新增模块未实现。

- [x] **Step 3: 实现采集器和快照可用性**

`disk` 保持 M1 的聚合 `DiskSample { read_bps, write_bps }` 语义；新增可选 `filesystems: Vec<FilesystemSample>` 键提供 mount/total/available/used_pct，statvfs 失败只省略该挂载点。默认挂载点只含 `/`，用户可配置额外路径，排除重复设备重复累计。

`hwmon` 扫描 `hwmon*/name`、`temp*_label`、`temp*_input`，稳定排序并按 `chip/label` 设置 `TempSample.label`；CPU 温度写入 `temps`，不硬编码只有 temp1。新增 `uptime_s` 键供内置 `uptime` DisplayItem 使用。`power` 写入新增可选 `battery` 键。CPU `freq_mhz` 和 `load` 在读取成功时填值，失败保留 None。

所有新字段加入 D-Bus 字典时遵循“增加可选键、不改旧键”；未采集的结构省略对应键，采集器可用但列表确实为空时才使用空列表，CLI JSON 将省略结构映射为 null。另提供只读 `CollectorStatus(a{sv})`，值包含 `available` 和 `reason`，让设置界面能区分零值、空列表与不可用。

- [x] **Step 4: 验证**

Run: `cargo test -p trafficview-collectors && cargo test --workspace`

Expected: 全部 PASS；无电池、无 cpufreq 权限或挂载点消失时其他采样仍更新，D-Bus v1 旧键测试不变。

- [x] **Step 5: 提交**

```bash
git add crates/collectors crates/core/src/lib.rs crates/daemon/src/service.rs crates/daemon/src/dbus.rs Cargo.lock
git commit -m "feat: collect disk hardware and power metrics"
```

### Task 2: 实现 AMD、NVIDIA 和 Intel GPU 后端

**Files:**
- Create: `crates/collectors/src/gpu.rs`
- Modify: `crates/collectors/src/lib.rs` — 导出 gpu
- Modify: `crates/core/src/lib.rs`
- Modify: `crates/daemon/src/service.rs`

- [x] **Step 1: 写三类后端解析与不可用失败测试**

AMD fixture 覆盖 `gpu_busy_percent`、`mem_info_vram_used` 和文件不存在。NVIDIA fixture 覆盖 NVML 成功、驱动不匹配、NVML 不可加载，以及 `nvidia-smi --query-gpu=index,name,utilization.gpu,memory.used,temperature.gpu --format=csv=noheader,nounits` 的解析。Intel fixture 覆盖 `/proc/<pid>/fdinfo` 的 `drm-engine-*` 纳秒累计、重复 fd 去重、进程退出和首帧基线。

```rust
#[test]
fn intel_utilization_uses_monotonic_engine_deltas() {
    let before = parse_intel_fdinfo("drm-engine-render: 1000000000 ns\n").unwrap();
    let after = parse_intel_fdinfo("drm-engine-render: 1500000000 ns\n").unwrap();
    assert_eq!(intel_engine_usage(before, after, Duration::from_secs(1)), Some(50.0));
}
```

- [x] **Step 2: 确认失败**

Run: `cargo test -p trafficview-collectors gpu::`

Expected: FAIL，GPU 后端未实现。

- [x] **Step 3: 实现后端优先级和节流**

AMD 直接读 drm sysfs；NVIDIA 优先 NVML 动态库，加载失败才允许 nvidia-smi，子进程 2 秒超时且不得阻塞 Tokio worker；Intel 不拿 `gt_cur_freq_mhz` 冒充利用率，只将频率作为新增可选字段，利用率来自 engine fdinfo 差分。Intel 全进程扫描默认每 5 秒一次；中间帧保留最后读数但设置 `stale=true` 并携带原 `observed_at_ms`，既不能伪装成新采样，也不让显示每秒闪烁为 `--`，超过 15 秒未更新才返回 None。

多 GPU 使用新增 `gpus: Vec<GpuSample>`，原 `gpu` 保持“主 GPU”兼容别名；主 GPU 按配置 stable PCI id 选择，未配置取第一块。权限不足、驱动不存在和 GPU 被移除均更新 `CollectorStatus`，不清空其他指标。

- [x] **Step 4: 验证真实硬件或记录不可测试边界**

Run: `cargo test -p trafficview-collectors gpu:: && cargo test --workspace`

Expected: fixture 全部 PASS。每种实际可用硬件分别对照 `intel_gpu_top`、`nvidia-smi` 或 `radeontop` 同窗口读数；不存在的硬件必须在验收记录标为未实测，不能声称通过。

- [x] **Step 5: 提交**

```bash
git add crates/collectors/src/gpu.rs crates/collectors/src/lib.rs crates/core/src/lib.rs crates/daemon/src/service.rs
git commit -m "feat: support AMD NVIDIA and Intel GPU metrics"
```

### Task 3: 实现 `trafficview-plugin-v1` 子进程协议与宿主

**Files:**
- Create: `crates/plugin-proto/Cargo.toml`
- Create: `crates/plugin-proto/src/lib.rs`
- Create: `crates/daemon/src/plugins.rs`
- Create: `crates/daemon/tests/plugins.rs`
- Modify: `crates/daemon/src/service.rs`
- Modify: `crates/core/src/config.rs`
- Modify: `Cargo.toml`

- [x] **Step 1: 写协议帧与宿主生命周期失败测试**

协议测试覆盖空 items、重复 id、非法 id、未知消息字段、超长行、非 UTF-8、插件值中换行。宿主测试用 Linux fixture 命令模拟：正常响应、2 秒超时、立即崩溃、写 stderr、连续三次失败后禁用、reload 后允许重试。

```rust
#[test]
fn plugin_values_become_first_class_snapshot_items() {
    let response = PluginResponse {
        items: vec![PluginItem { id: "fan_rpm".into(), name: "FAN".into(), unit: "rpm".into(), format: "{value} rpm".into() }],
        values: BTreeMap::from([("fan_rpm".into(), "1420".into())]),
    };
    let values = response.validated_values("demo").unwrap();
    assert_eq!(values.get("demo.fan_rpm").map(String::as_str), Some("1420"));
}

#[test]
fn response_over_one_mebibyte_is_rejected_before_json_parsing() {
    assert!(validate_frame_length(1_048_577).is_err());
}
```

- [x] **Step 2: 确认失败**

Run: `cargo test -p trafficview-plugin-proto && cargo test -p trafficview-daemon --test plugins`

Expected: FAIL，协议 crate 与宿主未实现。

- [x] **Step 3: 实现协议、发现和监督**

协议使用单行 JSON，daemon request 固定包含 `t`（Unix 秒浮点）和 `interval_ms`；response 使用调研中的 `items/values`。每个插件 frame 上限 1MiB，stdout 一行一帧，stderr 由独立任务读取并写 tracing，防止管道填满。

manifest 位于 `$XDG_DATA_HOME/trafficview/plugins/<id>/plugin.toml`：

```toml
id = "example"
command = "/absolute/path/to/plugin"
args = []
interval_ms = 1000
capabilities = ["metrics"]
```

command 直接 exec，不经过 shell；拒绝相对 command、id 与目录不一致、interval 小于 500ms。宿主按各自 interval 发请求，超过 2 秒 kill 并指数退避 1/2/4/8/16/32/60 秒；连续三次失败禁用，显式 `ReloadConfig` 才重试。慢插件不阻塞内置采集。Snapshot `plugin` key 使用 `<plugin-id>.<item-id>`，display registry 同步生成 `plugin.<plugin-id>.<item-id>` 项，插件消失时保留皮肤布局但显示 `--`。

- [x] **Step 4: 验证隔离与显示**

Run: `cargo test -p trafficview-plugin-proto && cargo test -p trafficview-daemon --test plugins && cargo test --workspace`

Expected: 全部 PASS；fixture 插件崩溃、超时或写垃圾 stdout 时，网络/CPU/内存仍按 1 秒周期更新，D-Bus 不被锁死。

- [x] **Step 5: 提交**

```bash
git add Cargo.toml Cargo.lock crates/plugin-proto crates/daemon/src/plugins.rs crates/daemon/tests/plugins.rs crates/daemon/src/service.rs crates/core/src/config.rs
git commit -m "feat: host isolated JSON metrics plugins"
```

### Task 4: 实现可选 eBPF 与 AF_PACKET 进程流量降级

**Files:**
- Create: `crates/daemon/src/process_traffic.rs`
- Create: `crates/daemon/src/bin/trafficviewd-ebpf.rs`
- Create: `crates/netprobe-ebpf/Cargo.toml`
- Create: `crates/netprobe-ebpf/src/lib.rs`
- Modify: `crates/daemon/Cargo.toml`
- Modify: `crates/core/src/config.rs`
- Modify: `crates/core/src/lib.rs`
- Modify: `crates/daemon/src/service.rs`
- Create: `packaging/systemd/trafficviewd-ebpf.service`
- Create: `packaging/systemd/trafficviewd-afpacket.service`
- Create: `packaging/dbus/com.trafficview.Ebpf1.conf`

- [ ] **Step 1: 写能力检测、聚合与降级失败测试**

`process_traffic.backend=off|ebpf|af_packet|auto` 默认 off。测试 auto 选择顺序、helper name owner 消失、capabilities 缺失、PID 重用、进程退出清理、map 计数器 reset，以及两个不同 UID 调用者只能看到各自进程。D-Bus system 接口定义为：

```text
NAME: com.trafficview.Ebpf1
PATH: /com/trafficview/Ebpf1
Methods: GetProcesses() -> a{sv}, GetBackendStatus() -> a{sv}
Signals: 无；user daemon 每个采样周期调用一次 GetProcesses()
```

新增 Snapshot 键 `processes` 是可选数组，每项 `pid/name/rx_bps/tx_bps/rx_total/tx_total`；默认 off 时 D-Bus 省略该键、CLI JSON 输出 `processes=null`，可用但无匹配进程时才输出空数组。

- [ ] **Step 2: 确认默认测试与 feature 构建边界**

Run: `cargo test -p trafficview-daemon process_traffic:: && cargo check -p trafficview-daemon --no-default-features`

Expected: 功能测试先 FAIL；默认无 feature 构建必须保持可执行，不引用 aya 类型。

- [ ] **Step 3: 实现 eBPF helper 与 system D-Bus**

eBPF crate 独立于 host stable workspace，按 aya 当前稳定工具链文档固定 `bpf-linker` 和 nightly；host `process-ebpf` feature 从 `TRAFFICVIEW_EBPF_OBJECT`（开发）或 `/usr/lib/trafficview/netprobe.bpf.o`（安装）运行时加载对象，不用 `include_bytes!` 强制普通 check 依赖生成物，也不在普通 `cargo test --workspace` 中构建内核程序。内核侧使用 per-CPU 或普通 hash map 按 `pid+uid` 聚合 TCP 与 UDP 收发字节，并另记 cgroup 供容器维度扩展，覆盖 `tcp_sendmsg/tcp_cleanup_rbuf` 和 UDP 发送/接收路径；user helper 每秒迭代 map 并差分，按 PID 读取 comm，进程退出后及时清理，PID start time 用于防 PID 重用串数据。

systemd unit 最小授权：

```ini
[Service]
Type=dbus
BusName=com.trafficview.Ebpf1
ExecStart=/usr/libexec/trafficviewd-ebpf --backend ebpf
CapabilityBoundingSet=CAP_BPF CAP_PERFMON CAP_NET_ADMIN
AmbientCapabilities=CAP_BPF CAP_PERFMON CAP_NET_ADMIN
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
```

system D-Bus policy 允许 helper 拥有名称和普通用户调用只读方法，不允许用户修改 map 或启动特权操作。helper 必须通过 `GetConnectionUnixUser` 识别调用者 UID，只返回该 UID 拥有的进程；内核 map key 包含 `pid+uid`，user helper 再校验 `/proc/<pid>` 的 UID 与 start time，root 才能请求全部用户。不得广播包含全系统进程名的公共信号。user daemon 只取当前用户 Top N 聚合，不接收每包事件。

- [ ] **Step 4: 实现 AF_PACKET 降级**

`--backend af-packet` 使用 `CAP_NET_RAW`，抓包只累计字节并以五元组/inode 映射 `/proc/net/{tcp,udp}` 与 `/proc/*/fd`；inode→PID 缓存短周期更新，丢包和无法归属流量进入独立 `unattributed`，不能分配给随机 PID。新增 `trafficviewd-afpacket.service`，其 `ExecStart=/usr/libexec/trafficviewd-ebpf --backend af-packet`，`CapabilityBoundingSet` 与 `AmbientCapabilities` 只含 `CAP_NET_RAW`，其余沙箱项与 eBPF unit 相同；两个 unit 使用同一 BusName，必须互斥启用。auto 仅在 eBPF 能力/内核不支持时尝试 AF_PACKET；两者均不可用时 `CollectorStatus` 给出缺少的能力和可选 system service 安装提示。

分别构建并验证：

```bash
rustup toolchain install nightly
cargo install bpf-linker
cargo +nightly build --manifest-path crates/netprobe-ebpf/Cargo.toml --release --target=bpfel-unknown-none -Z build-std=core
cp crates/netprobe-ebpf/target/bpfel-unknown-none/release/libtrafficview_netprobe.so target/trafficview-netprobe.bpf.o
TRAFFICVIEW_EBPF_OBJECT="$PWD/target/trafficview-netprobe.bpf.o" cargo check -p trafficview-daemon --features process-ebpf
cargo test -p trafficview-daemon process_traffic::
systemd-analyze verify packaging/systemd/trafficviewd-ebpf.service
systemd-analyze verify packaging/systemd/trafficviewd-afpacket.service
```

Expected: 默认构建无 aya 依赖；eBPF 测试机产生已知 TCP/UDP 流量时 PID 排行与对照工具同窗口误差可解释；无 capabilities 时基础 Snapshot 持续更新且 `processes=null`。

- [ ] **Step 5: 提交**

```bash
git add crates/daemon/src/process_traffic.rs crates/daemon/src/bin crates/daemon/Cargo.toml crates/netprobe-ebpf crates/core/src/config.rs crates/core/src/lib.rs packaging/systemd packaging/dbus Cargo.lock
git commit -m "feat: add optional privileged per-process network metrics"
```

### Task 5: 交付独立 GTK4 设置界面并完成 M4 验收

**Files:**
- Create: `frontends/settings/Cargo.toml`
- Create: `frontends/settings/src/main.rs`
- Create: `frontends/settings/src/window.rs`
- Create: `frontends/settings/src/dbus.rs`
- Modify: `crates/daemon/src/dbus.rs`

- [x] **Step 1: 先增加配置 API 契约失败测试**

D-Bus 增加 `GetConfig() -> a{sv}` 和 `UpdateConfig(a{sv})`；Update 必须在 daemon 内完整校验后原子保存并广播 ConfigChanged，任一字段非法时整个更新不生效。测试两个客户端并发写不同字段不会产生部分 TOML，settings 不得直接写 daemon 配置文件。

- [ ] **Step 2: 确认失败并实现独立设置窗口**

Run: `dbus-run-session -- cargo test -p trafficview-daemon --test dbus`

Expected: 新 API 测试先 FAIL。实现后在 `frontends/settings` 建独立 `[workspace]`，主根 workspace 使用 `exclude=["frontends/settings","crates/netprobe-ebpf"]`，因此根构建不解析 GTK 依赖。

设置窗口页面固定为 General（周期/自启动）、Network（网卡与规则）、Display（皮肤/显示项/透明度/位置）、Alerts、History、Plugins/Advanced。保存按钮一次性调用 UpdateConfig；无 daemon 时全部编辑控件禁用并显示重连；CollectorStatus 展示不可用原因，不提供自动提权按钮。进程流量页只能请求配置 backend，system helper 安装仍由管理员完成。

- [ ] **Step 3: 运行默认与可选质量门**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo check -p trafficview-daemon --features process-ebpf
cargo build --workspace --release
cargo check --manifest-path frontends/settings/Cargo.toml
```

Expected: 主 workspace 在无 GTK 开发包的构建环境仍成功；有 GTK4/libadwaita 的环境 settings check 成功；所有默认测试通过。

- [ ] **Step 4: 执行权限与 UI 降级验收**

分别在无硬件、无 NVML、无 notification portal、无 plugin、无 eBPF capabilities、helper 崩溃和设置 daemon 断连场景运行。

Expected: 每种场景都有准确 CollectorStatus，基础指标无中断；设置修改在一个事务内生效并被 M1/M2 前端接收；不存在 secret/root password 输入或隐式 pkexec。

- [ ] **Step 5: 提交并关闭 M4**

```bash
git add frontends/settings crates/daemon/src/dbus.rs Cargo.toml Cargo.lock
git commit -m "feat: add optional GTK settings frontend"
```

M4 完成条件：所有默认可测试能力通过，实际存在的 GPU/eBPF 硬件完成实测，不存在的平台明确标记未实测；所有可选能力关闭时产品仍等同 M3 且性能无回归。

---

## 执行记录（2026-11-22，本机 VirtualBox / Cinnamon X11）

Task 1 已完成并以真实输出验证：

- 新增测试先失败（36 个编译错误），实现后 `cargo test -p trafficview-collectors`
  27 项、`cargo test -p trafficview-core` 16 项通过。
- `dbus-run-session -- cargo test --workspace` 全部通过；`cargo fmt --all -- --check`、
  `cargo clippy --workspace --all-targets -- -D warnings` 与 release 构建均通过。
- release daemon 重启后 `GetSnapshot` 实际包含 `disk`、`filesystems`、`uptime_s`
  与 `cpu.load`（也包含 `cpu.freq_mhz` 的缺省省略）；`CollectorStatus` 返回
  `disk/filesystems/uptime/loadavg` 可用，`battery/temps/cpufreq` 不可用及原因。
- 本机无电池；`/sys/class/hwmon/hwmon0` 实际是 AC 适配器，没有温度输入；
  `/sys/devices/system/cpu/cpufreq` 为空。三项均按 `available=false` + 原因降级，
  未伪造零值。
- Ruling：额外挂载点配置暂不做，先固定 `/`，留到 Task 5 的配置 API；
  代价是 Task 1 期间只能用默认挂载点，设置界面实现时补齐。

Task 2 已完成并以真实输出验证：

- 新增 7 项 GPU 测试先失败、后全部通过：AMD `gpu_busy_percent`/VRAM 与缺失计数、
  `nvidia-smi` CSV 解析与驱动错误、Intel fdinfo 引擎差分/重复 fd 去重/
  `stale` 与 15 秒过期、无受支持后端时的原因。
- `dbus-run-session -- cargo test --workspace` 全部通过（collectors 34 项）；
  `fmt`、`clippy -D warnings` 与 release 构建通过。
- release daemon 重启后 `CollectorStatus.gpu` 返回
  `available=false, reason="no supported GPU backend"`，`gpu`/`gpus` 键按约定省略。
- 本机是 VirtualBox 虚拟显卡，无 AMD/NVIDIA/Intel GPU，无法对照
  `radeontop`/`nvidia-smi`/`intel_gpu_top`；三类后端实机读数标记为未实测。
- Ruling：主 GPU 暂取第一块，按配置 stable PCI id 的选择留到 Task 5 配置 API；
  NVML 通过 libc `dlopen` 调用，失败回退 `nvidia-smi`（2 秒超时）。

Task 3 已完成并以真实输出验证：

- 新建 `trafficview-plugin-proto`：10 项协议测试覆盖空 items、重复/非法 id、
  未知字段、超长帧在 JSON 解析前拒绝、非 UTF-8、值内换行、未声明 value、
  request 往返与快照键前缀；宿主 9 项测试先全部失败（`not implemented`），
  实现后通过：正常响应、2 秒超时、崩溃退出码、stderr 大量输出不死锁、
  三次失败禁用、退避饱和、清单发现校验、reload 重试、查询不阻塞调用方。
- 协议为单行 JSON，request 固定 `t/interval_ms`，response 为 `items/values`；
  单帧 1 MiB，超长在 UTF-8/JSON 解析前拒绝。清单位于
  `$XDG_DATA_HOME/trafficview/plugins/<id>/plugin.toml`，command 必须绝对路径，
  id 与目录一致，interval 下限 500ms。
- 每个插件使用独立 OS 线程；stdout 单帧读取与 stderr 排空各用独立线程，
  超时 kill，失败退避 1/2/4/8/16/32/60 秒，连续 3 次失败禁用，
  `ReloadConfig` 重新发现并清零失败计数。
- 调度器把成功值写入 `Snapshot.plugin`（键 `<plugin-id>.<item-id>`），并在
  `CollectorStatus` 增加 `plugin.<id>`；失败只移除该插件值，不影响内置指标。
- `dbus-run-session -- cargo test --workspace` 全部通过（158 项）；
  `cargo fmt --all`、`cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo build --release -p trafficview-daemon` 通过。
- 独立 session bus 实测：fixture 插件返回 `{'demo.fan_rpm': '1420'}`；
  `CollectorStatus` 中 `plugin.demo.available=true`，崩溃插件
  `plugin.bad.available=false, reason="plugin exited with status 7"`，
  日志显示 1s/2s 两次退避后第三次失败 `disabled=true`；同帧
  `net/cpu/mem/disk/filesystems` 仍正常输出，D-Bus 未被阻塞。
- 未实测边界：没有真实第三方插件二进制，全部用 `/bin/sh` fixture；插件宿主
  仅面向 Linux。P95 插件长尾延迟与真实插件生态兼容性待后续版本验证。

Task 5 Step 1 已完成（仅配置 API；GTK 窗口未开始）：

- 新增 `GetConfig() -> a{sv}` 与 `UpdateConfig(a{sv})`，覆盖全部 v1 配置字段；
  顶层部分更新、嵌套小节按键合并、`rules/items` 数组整体替换。
- `UpdateConfig` 在副本上合并并调用 `Config::validate`，再校验网卡存在性；
  持写锁原子保存配置后才提交内存状态，失败时内存与磁盘都不变。
- 新增 2 项 daemon 单元测试（完整往返、未知键拒绝）与 2 项 D-Bus 集成测试：
  非法批次整体回滚、两个连接并发写不同字段不丢字段且 TOML 可重新解析。
- `dbus-run-session -- cargo test -p trafficview-daemon` 通过；
  `cargo clippy -p trafficview-daemon --all-targets -- -D warnings` 通过。
- 只修改配置 API 与文档；`frontends/settings` 尚未创建，GTK 依赖未引入。

Task 4 部分完成（daemon 消费者与降级契约；特权 helper 未实现）：

- 配置新增 `[process_traffic] backend = "off"`（`off|ebpf|af_packet|auto`），
  默认 off；`GetConfig`/`UpdateConfig` 同步暴露该小节。
- `Snapshot` 新增可选 `processes` 键（`av`，每项
  `pid/name/rx_bps/tx_bps/rx_total/tx_total`）；默认 off 时省略，CLI JSON 为 null。
- daemon 侧异步消费者：system bus `NameHasOwner` 检查、`GetBackendStatus`
  能力协商、`GetProcesses` 解析、Top 10 排序、1 秒查询超时与失败清空；
  `auto` 优先 eBPF，能力不足再 AF_PACKET，缺失时给出 systemd 安装提示。
- 新增 9 项 `process_traffic` 单元测试，以及 D-Bus 集成断言（默认省略、
  `CollectorStatus.processes` 关闭原因、配置往返与非法 backend）。
- 质量门通过：`cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo check -p trafficview-daemon --no-default-features`、
  `dbus-run-session -- cargo test --workspace`。
- 未完成：`trafficviewd-ebpf` helper、eBPF/AF_PACKET 内核侧实现、两个 systemd
  unit 与 system bus policy。本机无 `CAP_BPF/CAP_NET_RAW` 且 sudo 需密码，
  helper 实测、UID 隔离、PID 重用与 map reset 均标记未实测。
