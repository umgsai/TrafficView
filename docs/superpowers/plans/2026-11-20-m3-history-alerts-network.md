# TrafficView M3 历史、告警与网卡策略 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 增加连续可恢复的日月流量统计、可抑制的桌面告警、多网卡自动选择和系统深浅色跟随。

**Architecture:** daemon 内新增独立 `HistoryStore`、`AlertEngine` 和 `NetSelector`。速率仍使用单调时钟短差分；历史累计使用网卡原始 64 位字节计数器的非负增量，两者不共享“休眠帧丢弃”语义。SQLite 事务同时写分钟、日和网卡游标，前端只通过 D-Bus 查询。

**Tech Stack:** M2 + rusqlite(bundled)、chrono、notify-rust、rtnetlink、netlink-packet-route、globset、zbus portal。

**前置条件:** M2 验收完成。历史测试使用临时数据库与显式时区；网络选择测试全部使用 fixture，真实热插拔只作为最终集成验收。

---

## 文件结构

- Create: `crates/daemon/src/history.rs` — schema、迁移、累计、rollup、保留策略。
- Create: `crates/daemon/src/alerts.rs` — 规则、持续时长、冷却、静默时段。
- Create: `crates/daemon/src/network.rs` — 网卡枚举、默认路由、黑白名单、热插拔。
- Create: `crates/daemon/src/theme.rs` — freedesktop appearance portal。
- Modify: `crates/daemon/src/service.rs` — 接入历史、告警、网卡事件。
- Modify: `crates/daemon/src/dbus.rs` — 历史查询、网卡列表详情、Alert 信号。
- Modify: `crates/core/src/config.rs` — `history/alerts/interfaces/theme` 配置。
- Modify: `crates/core/src/lib.rs` — `InterfaceInfo`、`TrafficSummary`、`AlertEvent`。
- Modify: `crates/collectors/src/net.rs` — 全接口计数器采集与原始增量接口。
- Create: `crates/collectors/src/netlink.rs` — RTM_GETLINK/IFLA_STATS64 主数据源。
- Create: `crates/core/src/smoothing.rs` — 可选三点滑动平均与 EMA。
- Modify: `crates/cli/src/main.rs` — `history` 和 `interfaces` 子命令。
- Modify: `crates/render/src/skin.rs` — 可选 light 背景。
- Modify: `crates/render/src/lib.rs` — 主题进入缓存键。
- Create: `crates/daemon/tests/history.rs`
- Create: `crates/daemon/tests/alerts.rs`
- Create: `crates/daemon/tests/network.rs`

### Task 1: 实现可迁移的 SQLite 历史存储

**Files:**
- Create: `crates/daemon/src/history.rs`
- Create: `crates/daemon/tests/history.rs`
- Modify: `crates/core/src/config.rs`

- [x] **Step 1: 写建库、累计和重置失败测试**

```rust
#[test]
fn minute_and_day_totals_are_updated_in_one_transaction() {
    let mut store = HistoryStore::open_memory().unwrap();
    let id = store.upsert_interface("mac:aa", "eth0").unwrap();
    store.add_delta(id, at("2026-11-20T10:00:20Z"), 1024, 2048).unwrap();
    store.add_delta(id, at("2026-11-20T10:00:50Z"), 1024, 0).unwrap();
    assert_eq!(store.minute(id, "2026-11-20T10:00Z").unwrap(), (2048, 2048));
    assert_eq!(store.day(id, "2026-11-20").unwrap(), (2048, 2048));
}

#[test]
fn counter_reset_rebases_without_negative_or_duplicate_totals() {
    let mut store = HistoryStore::open_memory().unwrap();
    let id = store.upsert_interface("mac:aa", "eth0").unwrap();
    store.observe(id, at("2026-11-20T10:00:00Z"), 1000, 2000).unwrap();
    store.observe(id, at("2026-11-20T10:01:00Z"), 1500, 2600).unwrap();
    store.observe(id, at("2026-11-20T10:02:00Z"), 10, 20).unwrap();
    store.observe(id, at("2026-11-20T10:03:00Z"), 110, 220).unwrap();
    assert_eq!(store.day(id, "2026-11-20").unwrap(), (600, 800));
}
```

另写测试：同一进程重启后从持久化游标继续；计数器相等不写增量；同一分钟 UPSERT 不重复；`PRAGMA user_version` 重开不重复迁移；非法 schema 版本返回可诊断错误。

- [x] **Step 2: 确认失败**

Run: `cargo test -p trafficview-daemon --test history`

Expected: FAIL，`HistoryStore` 未定义。

- [x] **Step 3: 实现调研 schema 与累计算法**

执行以下 v1 schema，并设置 `PRAGMA journal_mode=WAL`、`foreign_keys=ON`、`busy_timeout=5000`：

```sql
CREATE TABLE iface(
  id INTEGER PRIMARY KEY,
  stable TEXT UNIQUE NOT NULL,
  name TEXT NOT NULL,
  last_rx INTEGER,
  last_tx INTEGER
);
CREATE TABLE traffic_min(
  iface_id INTEGER NOT NULL,
  ts INTEGER NOT NULL,
  rx INTEGER NOT NULL DEFAULT 0,
  tx INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(iface_id, ts)
) WITHOUT ROWID;
CREATE TABLE traffic_day(
  iface_id INTEGER NOT NULL,
  ymd TEXT NOT NULL,
  rx INTEGER NOT NULL DEFAULT 0,
  tx INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(iface_id, ymd)
) WITHOUT ROWID;
PRAGMA user_version=1;
```

稳定键优先为 `mac:<小写MAC>`，无永久 MAC 时才使用 `name:<接口名>`；ifindex 只用于当次事件关联，不作为跨重启唯一键。发现同 MAC 新名字时更新 `iface.name`，保留同一历史行。

`observe` 的完整规则：首次只见游标；当前值均不小于上次值时，将两个差值在同一事务中加到分钟行、日行并更新游标；任一计数器变小时只重置两个游标，不添加差值。SQLite 有符号 INTEGER 与 Rust u64 的转换必须在 `i64::MAX` 处明确报错，禁止 wrap/cast 为负数。

每分钟时间戳使用 UTC Unix 秒且向下取整到 60 秒；日键按用户配置时区生成，默认系统本地时区。`history.retention_days=90` 默认保留 90 天明细，日汇总不随明细清理而删除。

- [x] **Step 4: 验证数据库语义**

Run: `cargo test -p trafficview-daemon --test history && cargo test -p trafficview-daemon`

Expected: 全部 PASS；测试重复打开同一数据库不出现锁死或重复累计。

- [x] **Step 5: 提交**

```bash
git add crates/daemon/src/history.rs crates/daemon/tests/history.rs crates/core/src/config.rs Cargo.lock
git commit -m "feat: persist per-interface traffic history in SQLite"
```

### Task 2: 接入守护进程并提供日月查询

**Files:**
- Modify: `crates/daemon/src/service.rs`
- Modify: `crates/daemon/src/dbus.rs`
- Modify: `crates/core/src/lib.rs`
- Modify: `crates/cli/src/main.rs`
- Modify: `crates/daemon/tests/history.rs`

- [x] **Step 1: 写 daemon 重启和 D-Bus 查询失败测试**

集成测试使用临时 XDG data 目录和固定网卡 fixture：采样两个周期 → 停 daemon → 推进原始计数器 → 重启 → 再采样。断言历史只增加所有非负差值一次，速率首帧仍重新建立单调时钟基线。D-Bus 测试调用：

```text
GetTrafficSummary(scope="day"|"month", iface_stable="") -> a{sv}
```

返回键固定为 `scope/start/end/rx/tx/interfaces`；`interfaces` 为各网卡分解，空 stable 表示所有网卡求和。无效 scope 返回 D-Bus error，不能默认变成全部历史。

- [x] **Step 2: 确认失败**

Run: `dbus-run-session -- cargo test -p trafficview-daemon --test history`

Expected: FAIL，daemon 尚未接入 store 和查询方法。

- [x] **Step 3: 实现接入和 CLI**

数据库路径为 `$XDG_DATA_HOME/trafficview/history.db`，未设置时使用 `$HOME/.local/share/trafficview/history.db`。每个原始计数器采样都调用 `observe`，由 `observe` 在同一个短事务中 UPSERT 当前分钟、当天和游标，因此干净退出无需额外 flush；每分钟只保留一行并不意味着延迟到分钟末才写盘。`add_delta` 是 `observe` 复用的事务辅助函数。每天首次采样时清理 90 天前 `traffic_min`，避免每秒执行 retention。

CLI 增加：

```text
trafficview interfaces --json
trafficview history today [--iface <stable>] [--json]
trafficview history month [--iface <stable>] [--json]
```

人类可读输出使用 1024 进制，`--json` 保留原始字节整数和 stable/name，不能先格式化再解析。

- [x] **Step 4: 验证跨重启和并发消费者**

Run: `dbus-run-session -- cargo test -p trafficview-daemon --test history && cargo test --workspace`

Expected: 全部 PASS；CLI 查询与直接 SQLite 求和一致，前端查询期间采样线程不会被长时间阻塞。

- [x] **Step 5: 提交**

```bash
git add crates/daemon crates/core/src/lib.rs crates/cli/src/main.rs
git commit -m "feat: expose daily and monthly traffic summaries"
```

### Task 3: 实现多网卡自动选择与热插拔

**Files:**
- Create: `crates/daemon/src/network.rs`
- Create: `crates/daemon/tests/network.rs`
- Modify: `crates/collectors/src/net.rs`
- Create: `crates/collectors/src/netlink.rs`
- Create: `crates/core/src/smoothing.rs`
- Modify: `crates/core/src/lib.rs` — 导出 smoothing
- Modify: `crates/core/src/config.rs` — 增加 `network_smoothing`
- Modify: `crates/collectors/src/lib.rs` — 导出 netlink
- Modify: `crates/daemon/src/service.rs`
- Modify: `crates/daemon/src/dbus.rs`

- [x] **Step 1: 写选择策略失败测试**

```rust
#[test]
fn default_route_wins_before_rate_based_selection() {
    let interfaces = vec![iface("eth0", Up, 100), iface("wlan0", Up, 9000)];
    let selector = selector_with_default_route("eth0", &interfaces);
    assert_eq!(selector.choose(&rates(&interfaces)).unwrap().name, "eth0");
}

#[test]
fn fastest_active_interface_is_used_without_default_route() {
    let interfaces = vec![iface("eth0", Up, 100), iface("wlan0", Up, 9000), iface("lo", Up, 99999)];
    let selector = selector_without_default(&interfaces);
    assert_eq!(selector.choose(&rates(&interfaces)).unwrap().name, "wlan0");
}

#[test]
fn whitelist_overrides_blacklist_and_hotplug_rebases() {
    let interfaces = vec![iface("tun0", Up, 50), iface("eth0", Up, 10)];
    let selector = selector_with_whitelist("tun0", &interfaces);
    assert_eq!(selector.choose(&rates(&interfaces)).unwrap().name, "tun0");
}
```

补充接口消失、默认路由变化、全部 down、只有黑名单网卡、MAC 重命名和 USB 热插拔重建基线测试。netlink fixture 验证只采用 `IFLA_STATS64`，缺少 64 位属性时回退 procfs，不混用 32 位计数器。另在 `smoothing.rs` 写三点平均 `[100,200,300] -> 200`、EMA(0.3) 对 `[100,0] -> 70`、首帧不补零和配置 none 原样透传测试。

- [x] **Step 2: 确认失败**

Run: `cargo test -p trafficview-daemon --test network && cargo test -p trafficview-collectors netlink:: && cargo test -p trafficview-core smoothing::`

Expected: FAIL，`NetSelector`、netlink 主数据源与平滑器尚未定义。

- [x] **Step 3: 实现发现、选择和 netlink 事件**

从 `/sys/class/net/*` 读取 name、ifindex、MAC、operstate、carrier、type；采集器增加 `sample_all()`，优先用一次 rtnetlink `RTM_GETLINK` 请求读取全部接口的 `IFLA_STATS64`，失败或缺属性时才一次解析所有 `/proc/net/dev` 行，保留每卡原始计数器和速率。配置 `network_smoothing=none|moving_average_3|ema` 默认 none；EMA 固定 alpha=0.3，只处理有效网络速率帧且只影响展示快照，历史 `observe` 始终使用未平滑的原始计数器。选择顺序严格为：

1. 显式白名单且存在的接口；
2. 非黑名单的默认路由接口；
3. 多张活跃接口中当前 `rx_bps+tx_bps` 最大者；
4. 无活跃接口时返回 `NoActiveInterface`，保留 daemon 与历史查询。

默认黑名单匹配 `lo/docker*/veth*/br-*/virbr*/tun*`，白名单优先。订阅 rtnetlink `RTM_NEWLINK/RTM_DELLINK/RTM_NEWROUTE`，事件只标记 dirty 并合并到下一采样周期处理，避免事件风暴。切换接口时重置速率基线并发出 `InterfacesChanged(as)`；D-Bus 新增只读 `InterfaceDetails(a{sv})`，原 `Interfaces` 字符串列表保持兼容。

- [ ] **Step 4: 自动与真实热插拔验收**

Run: `cargo test -p trafficview-daemon --test network && cargo test --workspace`

Expected: 全部 PASS。真实 Linux 上禁用/启用非默认测试网卡或切换 WiFi，托盘列表在一个采样周期后更新，无旧网卡速率尖峰；历史仍归入相同 MAC 的 stable id。

- [x] **Step 5: 提交**

```bash
git add crates/daemon/src/network.rs crates/daemon/tests/network.rs crates/daemon/src/service.rs crates/daemon/src/dbus.rs crates/collectors/src/net.rs crates/collectors/src/netlink.rs crates/collectors/src/lib.rs crates/core/src/smoothing.rs crates/core/src/lib.rs crates/core/src/config.rs
git commit -m "feat: select and track Linux network interfaces"
```

### Task 4: 实现告警状态机与桌面通知

**Files:**
- Create: `crates/daemon/src/alerts.rs`
- Create: `crates/daemon/tests/alerts.rs`
- Modify: `crates/core/src/config.rs`
- Modify: `crates/daemon/src/service.rs`
- Modify: `crates/daemon/src/dbus.rs`
- Modify: `crates/tray/src/lib.rs`
- Modify: `crates/render/src/lib.rs`

- [x] **Step 1: 写持续时间、冷却和静默时段失败测试**

```rust
#[test]
fn threshold_requires_consecutive_samples() {
    let mut engine = AlertEngine::new(rule_cpu(80.0, 3, 300));
    assert!(engine.observe_cpu(at_secs(0), 90.0).is_empty());
    assert!(engine.observe_cpu(at_secs(1), 95.0).is_empty());
    assert_eq!(engine.observe_cpu(at_secs(2), 91.0)[0].code, "cpu.high");
    assert!(engine.observe_cpu(at_secs(3), 91.0).is_empty());
}

#[test]
fn recovery_rearms_rule_and_quiet_hours_delay_notification() {
    let mut engine = AlertEngine::new(rule_cpu(80.0, 1, 300));
    assert_eq!(engine.observe_cpu(at_secs(0), 90.0).len(), 1);
    assert!(engine.observe_cpu(at_secs(1), 50.0).is_empty());
    assert_eq!(engine.observe_cpu(at_secs(400), 90.0).len(), 1);
}
```

测试覆盖 `net.up/net.down/cpu.high/mem.high/month.quota/net.disconnected`、冷却边界、跨午夜静默时段、配置 reload 清理不再存在的规则。状态机事件与通知发送分离，以便测试不依赖桌面。

- [x] **Step 2: 确认失败**

Run: `cargo test -p trafficview-daemon --test alerts`

Expected: FAIL，`AlertEngine` 未实现。

- [x] **Step 3: 实现规则与通知适配器**

配置结构每条规则包含 `enabled/metric/op/threshold/duration_s/cooldown_s/quiet_hours`。默认仅启用示例为全部 disabled，避免升级后突然打扰；月配额单位字节。指标连续达到条件才进入 active，恢复后清除持续计数；active 期间不重复通知，恢复后再次触发仍受 cooldown 限制。静默时段内事件更新状态但延迟通知，时段结束只发送仍 active 的最新一条。

通知适配器调用 `org.freedesktop.Notifications`，无服务时记 warning 且仍发送 D-Bus `Alert(level,code,message)`。托盘将最近告警显示在 tooltip，overlay 将对应 item 置为告警色；恢复事件清除颜色。告警逻辑不读取或修改渲染配置。

- [x] **Step 4: 验证**

Run: `cargo test -p trafficview-daemon --test alerts && cargo test --workspace`

Expected: 全部 PASS。真实桌面连续超阈值只出现一次通知，恢复后图标复原，无通知服务时 daemon 不退出。

- [x] **Step 5: 提交**

```bash
git add crates/daemon/src/alerts.rs crates/daemon/tests/alerts.rs crates/core/src/config.rs crates/daemon/src/service.rs crates/daemon/src/dbus.rs crates/tray crates/render
git commit -m "feat: add sustained metric alerts and desktop notifications"
```

### Task 5: 实现深浅色自动切换与 M3 总验收

**Files:**
- Create: `crates/daemon/src/theme.rs`
- Modify: `crates/core/src/config.rs`
- Modify: `crates/render/src/skin.rs`
- Modify: `crates/render/src/lib.rs`
- Modify: `crates/ui-x11/src/lib.rs`
- Modify: `crates/ui-wayland/src/lib.rs`

- [x] **Step 1: 写主题决策失败测试**

覆盖 `theme=light/dark/auto`、portal 返回 0/1/2、portal 不存在、light 背景缺失时回退默认背景、主题切换使 cache key 改变。portal 信号测试确保旧连接断开后不响应过期事件。

- [x] **Step 2: 确认失败并实现 portal 订阅**

Run: `cargo test -p trafficview-daemon theme:: && cargo test -p trafficview-render theme`

Expected: 先 FAIL。通过 `org.freedesktop.portal.Settings` 读取 `org.freedesktop.appearance/color-scheme`，订阅 `SettingChanged`；值 1=dark、2=light、0=unspecified。`auto` 在 unspecified/portal 不可用时回退 dark。daemon 通过新增只读属性 `ColorScheme s` 和已有 `ConfigChanged` 通知前端，前端不各自订阅 portal。

皮肤 `[canvas]` 增加可选 `background_light`；切换主题只改变渲染资源，不改变 item rect。X11/Wayland 收到配置变化后用新主题重绘，即使采样文本没变。

- [x] **Step 3: 运行完整质量门**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
dbus-run-session -- cargo test -p trafficview-daemon --test dbus
cargo build --workspace --release
```

Expected: 全部退出码为 0。

- [ ] **Step 4: 验证重启、休眠、清理与主题**

真实 Linux 验收顺序：产生已知流量 → 查询 today → 重启 daemon → 再产生流量 → 重启 OS → 再产生流量 → 挂起超过 5 秒恢复 → 切换系统深浅色。

Expected: 日汇总不重复且计数器 reset 不产生巨大值；恢复首帧无速率尖峰；90 天前分钟明细被清理而日汇总保留；两个图形前端在同一配置周期切换主题；portal 不可用时保持可读默认皮肤。

- [x] **Step 5: 提交并关闭 M3**

```bash
git add crates/daemon/src/theme.rs crates/core/src/config.rs crates/render crates/ui-x11 crates/ui-wayland
git commit -m "feat: follow the desktop color scheme"
```

M3 完成条件：数据库、告警、网络 fixture 测试全部通过，真实重启/休眠连续性和主题切换通过，M0–M2 无回归。

---

## 执行记录（2026-11-22，本机 Cinnamon / X11）

已完成并留下真实输出：

- `cargo test -p trafficview-daemon --test history`（13 项）、`--test network`（7 项）、
  `--test alerts`（6 项）、`cargo test --workspace` 与
  `dbus-run-session -- cargo test -p trafficview-daemon --test dbus`（4 项）全部通过。
- 真实 daemon 使用 netlink `IFLA_STATS64` 作为主数据源（日志
  `network counter baseline established source="netlink"`）；历史库写入
  `$XDG_DATA_HOME/trafficview/history.db`，`trafficview history today|month --json`
  返回真实字节增量。
- 停止临时 daemon 再启动后，`traffic_day` 累计继续增加、游标未清零。
- 用临时配置（`cpu_pct > 0.1`、`duration_s=1`）真实触发 `cpu.high`：
  `dbus-monitor` 捕获到 `Alert` 信号，以及发往 Cinnamon 通知服务的
  `org.freedesktop.Notifications.Notify`，内容为
  `TrafficView 告警 / CPU 使用率 … 超过阈值 0.1（持续 1 秒）`。
- overlay 的 X11/Wayland 前端现均已订阅既有 `Alert(sss)`，通过共享
  `AlertState` 将对应 item 置为 info/warning/critical 色，恢复后回到原皮肤或
  配置颜色；告警状态纳入缓存键，不改变 D-Bus 或渲染配置。新增
  `alert_events_map_to_items_and_recovery_clears_only_that_alert`、
  `skin_alert_changes_pixels_and_cache_then_recovery_restores_both`、
  `fixed_layout_alert_changes_pixels_and_cache_then_recovery_restores_both`
  三项测试；render/X11/Wayland 聚焦测试共 27 项通过。
- Cinnamon X11 用临时 `cpu_pct > 80` 规则真实跨越阈值，`xwd` 捕获 CPU 项从
  `#FFD43B` 变为 critical `#F44336`，恢复后回到 `#FFD43B`；总线/日志分别记录
  `80.7 超过阈值` 与 `已恢复（当前 69.7）`。另一次 `cpu_pct > 70` 验证在同一
  `dbus-monitor` 日志中捕获 critical `86.2 超过阈值` 和 info
  `已恢复（当前 7.9）`。临时规则已删除并执行 `ReloadConfig`。
- `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、
  `dbus-run-session -- cargo test --workspace` 与 `cargo build --workspace --release`
  全部通过；三个 systemd --user 单元使用新 release 产物干净重启，各仅一个实例。
- 通过 `gsettings set org.gnome.desktop.interface color-scheme prefer-light`
  实测 portal 值 0→2→0，daemon 日志与 `ColorScheme` 属性同步 dark→light→dark，
  已恢复原设置。深浅色观感仍需人工确认。

未完成/未验证（不得当作已验收）：

- 真实热插拔与自动切卡：本机只有 `lo` 和 `enp0s3` 两个接口，且无
  `CAP_NET_ADMIN`/sudo，无法创建 dummy/veth 或切换默认路由。选择顺序由
  fixture 测试覆盖，真实 daemon 只验证了自动选中默认路由 `enp0s3`。
- 桌面通知仅验证“通知服务收到 Notify 且内容正确”，弹窗外观未人工确认。
- Wayland/GNOME 的真实主题重绘与告警变色未在对应会话验收（本机无该会话）；
  当前只有编译、离屏渲染及前端单元测试证据。

收尾结论（2026-11-22）：除上述环境限制项外，M3 各任务已勾选；Task 3 Step 4
（真实热插拔）与 Task 5 Step 4（重启/休眠）因本机权限与验收条件不足保持未勾选，
不计入已验收范围。

