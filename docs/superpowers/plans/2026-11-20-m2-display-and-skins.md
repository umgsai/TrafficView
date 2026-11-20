# TrafficView M2 跨桌面展示与皮肤 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 交付原生皮肤系统、KDE/wlroots Wayland 悬浮窗、GNOME 顶栏扩展、Plasma widget 与 waybar 输出。

**Architecture:** `trafficview-render` 将 `skin.toml + PNG + Snapshot` 转成平台无关 ARGB 帧；X11 和 Wayland 只负责 surface 生命周期与输入。GNOME、Plasma 和 waybar 不复用像素画布，而是消费同一 D-Bus/CLI 快照并保留各自面板的无障碍和主题能力。

**Tech Stack:** M1 + tiny-skia、cosmic-text/fontdb、image、wayland-client、wayland-protocols-wlr、Smithay client toolkit、GJS/ESM、QML、serde_json。

**前置条件:** M1 完成且 D-Bus v1 不破坏。分别在 KDE Plasma 6/Wayland、GNOME 45+/Wayland、Sway 或 Hyprland/waybar 中验收；GNOME 不允许以 layer-shell 不可用为由跳过 Shell 扩展。

---

## 文件结构

- Create: `crates/render/src/skin.rs` — 原生皮肤 schema、校验、资源解析。
- Create: `crates/render/src/nine_patch.rs` — 九宫格缩放。
- Create: `crates/render/src/value.rs` — DisplayItem 取值、格式和阈值颜色。
- Modify: `crates/render/src/lib.rs` — 皮肤帧渲染与缓存。
- Create: `crates/render/tests/skin.rs`
- Create: `crates/render/tests/fixtures/minimal/skin.toml` — 无背景最小测试皮肤。
- Create: `crates/render/tests/fixtures/invalid/zero-width.toml`
- Create: `crates/render/tests/fixtures/invalid/duplicate-id.toml`
- Create: `crates/render/tests/fixtures/invalid/unsorted-thresholds.toml`
- Modify: `crates/core/src/format.rs` — 可选 bit/s 格式化。
- Modify: `crates/core/src/config.rs` — `RateUnit`，默认 bytes。
- Create: `crates/ui-wayland/Cargo.toml`
- Create: `crates/ui-wayland/src/lib.rs`
- Create: `crates/ui-wayland/src/layer_surface.rs`
- Create: `crates/ui-wayland/src/xdg_fallback.rs`
- Create: `crates/ui-wayland/tests/geometry.rs`
- Modify: `crates/cli/src/main.rs` — `skin validate/preview`、Wayland overlay、waybar 输出。
- Create: `frontends/gnome-extension/metadata.json`
- Create: `frontends/gnome-extension/extension.js`
- Create: `frontends/gnome-extension/stylesheet.css`
- Create: `frontends/gnome-extension/tests/controller_test.js`
- Create: `frontends/plasma-widget/metadata.json`
- Create: `frontends/plasma-widget/contents/ui/main.qml`
- Create: `frontends/plasma-widget/contents/ui/CompactRepresentation.qml`
- Create: `frontends/plasma-widget/contents/config/main.xml`

### Task 1: 实现并校验原生 `skin.toml`

**Files:**
- Create: `crates/render/src/skin.rs`
- Create: `crates/render/src/value.rs`
- Create: `crates/render/tests/skin.rs`
- Create: `crates/render/tests/fixtures/minimal/skin.toml`
- Modify: `crates/cli/src/main.rs`

- [ ] **Step 1: 写 schema、资源安全和阈值失败测试**

```rust
#[test]
fn parses_research_skin_model() {
    let skin = Skin::load(test_path("minimal")).unwrap();
    assert_eq!(skin.meta.name, "Minimal");
    assert_eq!(skin.items[0].id, "net_up");
    assert_eq!(skin.items[0].rect, [10, 6, 92, 20]);
    assert!(skin.items[0].mono);
}

#[test]
fn rejects_invalid_rectangles_and_thresholds() {
    for source in [
        include_str!("fixtures/invalid/zero-width.toml"),
        include_str!("fixtures/invalid/duplicate-id.toml"),
        include_str!("fixtures/invalid/unsorted-thresholds.toml"),
    ] {
        assert!(Skin::parse(source, Path::new("fixture")).is_err());
    }
}

#[test]
fn background_must_stay_inside_skin_directory() {
    let error = Skin::parse("[meta]\nname='x'\n[canvas]\nbackground='../../secret.png'", Path::new("fixture")).unwrap_err();
    assert!(error.to_string().contains("outside skin directory"));
}
```

测试 fixture 必须覆盖 `meta/canvas/[[item]]`、可选 `nine_patch`、默认字体/颜色、透明背景缺失、未知 item id 警告。未知 id 不应导致整个皮肤失败，但 `validate` 要输出逐条警告。

- [ ] **Step 2: 确认失败**

Run: `cargo test -p trafficview-render --test skin`

Expected: FAIL，`Skin` 和相关 fixture 尚未实现。

- [ ] **Step 3: 实现皮肤模型与 CLI 校验**

定义与调研一致的 `SkinMeta`、`Canvas`、`SkinItem`，`rect=[x,y,w,h]` 使用有符号 x/y 和无符号 w/h 的转换校验，拒绝零尺寸、画布溢出、非法颜色和未排序阈值。背景路径必须规范化后仍位于皮肤根目录；背景不存在时返回带路径的错误，不创建网络占位图。

CLI 增加：

```text
trafficview skin validate <skin-dir>
trafficview skin preview <skin-dir> --snapshot <snapshot.json> --output <preview.png>
```

`preview` 在离屏渲染中执行，无需图形会话；成功时退出码 0，警告写 stderr，stdout 只输出规范化皮肤摘要 JSON。`validate` 对所有 item id 使用 `core::DisplayItem` 的同一注册表，包含未来 `plugin.<id>` 动态命名空间。

- [ ] **Step 4: 验证**

Run: `cargo test -p trafficview-render --test skin && cargo test -p trafficview-cli skin`

Expected: 全部 PASS；有效皮肤生成非全透明 PNG，非法背景路径和重复 id 被明确拒绝。

- [ ] **Step 5: 提交**

```bash
git add crates/render crates/cli/src/main.rs Cargo.lock
git commit -m "feat: parse and validate native TrafficView skins"
```

### Task 2: 完成 PNG、九宫格、文本和阈值渲染

**Files:**
- Create: `crates/render/src/nine_patch.rs`
- Modify: `crates/render/src/lib.rs`
- Modify: `crates/render/src/value.rs`
- Modify: `crates/render/tests/skin.rs`
- Modify: `crates/core/src/format.rs`
- Modify: `crates/core/src/config.rs`

- [ ] **Step 1: 写像素不变量失败测试**

覆盖：透明 PNG 的 alpha 不被忽略；九宫格四个角像素不变；中心区域可拉伸；阈值在恰好 80/90 时切换颜色；`format="↑ {value}"` 只替换变量；CJK 字体缺失时回退而不是空字形；相同快照和皮肤产生相同 `cache_key`。

```rust
#[test]
fn threshold_boundaries_are_inclusive() {
    let item = SkinItem::threshold_test(&[(80.0, "#FFC107"), (90.0, "#F44336")]);
    assert_eq!(item.color_for(79.9), item.color);
    assert_eq!(item.color_for(80.0), "#FFC107");
    assert_eq!(item.color_for(90.0), "#F44336");
}

#[test]
fn formats_optional_bit_rates_with_binary_prefixes() {
    assert_eq!(trafficview_core::format::format_bit_rate(128), "1.0 Kb/s");
    assert_eq!(trafficview_core::format::format_bit_rate(1024), "8.0 Kb/s");
}

#[test]
fn unchanged_frame_is_not_rasterized_again() {
    let mut renderer = Renderer::new(test_skin());
    let first = renderer.render(&test_snapshot(), 1.0).unwrap();
    let second = renderer.render(&test_snapshot(), 1.0).unwrap();
    assert_eq!(first.cache_key, second.cache_key);
    assert_eq!(renderer.rasterize_count(), 1);
}
```

- [ ] **Step 2: 确认失败**

Run: `cargo test -p trafficview-render`

Expected: FAIL，九宫格、阈值和缓存尚未实现。

- [ ] **Step 3: 实现最小渲染管线**

顺序固定为：解析指标值 → 应用 format → 选择阈值颜色 → 背景 PNG/纯色 → 九宫格 → 按 item 顺序绘制文本 → 转 premultiplied ARGB。`value.rs` 默认使用 `core::format::format_rate` 处理 `net_up/net_down`；`overlay.units=bits` 时调用 `format_bit_rate(bytes_per_sec)`，先用 `bytes_per_sec.saturating_mul(8)` 再按相同 1024 进制格式化，默认 `bytes` 不改变 M1 行为。CPU/内存保留一位小数，未采集的可选值显示 `--`。`mono=true` 使用等宽数字特性；右对齐以 rect 右边为基准，不通过空格补齐。

九宫格 `nine_patch=[left,top,right,bottom]` 必须验证边距不超过原图尺寸；目标尺寸小于角区总尺寸时返回校验错误，不采样越界。缓存键包含快照显示值、皮肤内容 hash、DPI、透明度和主题，不包含原始计数器或单调时间戳，确保仅时间变化时不重绘。

- [ ] **Step 4: 自动测试及 X11 回归**

Run: `cargo test -p trafficview-render && cargo test --workspace`

Expected: 全部 PASS；M1 默认配置自动映射到内置 Default Dark 皮肤，XFCE/X11 的默认显示不回归。

- [ ] **Step 5: 提交**

```bash
git add crates/render
git commit -m "feat: render PNG skins with scalable text and thresholds"
```

### Task 3: 实现 Wayland layer-shell 与 GNOME 普通窗口降级

**Files:**
- Create: `crates/ui-wayland/Cargo.toml`
- Create: `crates/ui-wayland/src/lib.rs`
- Create: `crates/ui-wayland/src/layer_surface.rs`
- Create: `crates/ui-wayland/src/xdg_fallback.rs`
- Create: `crates/ui-wayland/tests/geometry.rs`
- Modify: `crates/cli/src/main.rs`
- Modify: `Cargo.toml`

- [ ] **Step 1: 写会话选择和几何失败测试**

```rust
#[test]
fn gnome_never_selects_layer_shell() {
    assert_eq!(Backend::choose("wayland", "GNOME", true), Backend::XdgFallback);
    assert_eq!(Backend::choose("wayland", "KDE", true), Backend::LayerShell);
    assert_eq!(Backend::choose("wayland", "sway", true), Backend::LayerShell);
}

#[test]
fn dragging_snaps_to_eight_pixel_grid() {
    let geometry = snap_anchor([103, 67], [1920, 1080], [240, 80], 8);
    assert_eq!(geometry.anchor, Anchor::TOP | Anchor::RIGHT);
    assert_eq!(geometry.margin.0 % 8, 0);
    assert_eq!(geometry.margin.1 % 8, 0);
}
```

还要覆盖多 output、1.0/1.25/1.5/2.0 fractional scale 的物理像素取整和空 input region。

- [ ] **Step 2: 确认失败**

Run: `cargo test -p trafficview-ui-wayland`

Expected: FAIL，Wayland crate 未实现。

- [ ] **Step 3: 实现平台 surface 生命周期**

`layer_surface.rs` 使用 `zwlr_layer_surface_v1`：`layer=overlay`、`exclusive_zone=-1`、anchor+margin、无键盘交互；`wl_shm` 提交渲染 ARGB，`set_input_region(empty)` 实现穿透。监听 `wl_output` 为选中的每个 output 建独立 surface；拖拽只改 anchor/margin 并吸附 8px，不尝试设置全局绝对坐标。

fractional-scale-v1 可用时按合成器 scale 渲染物理像素，不可用时使用整数 buffer scale；等待 frame callback 后再提交下一帧，避免无限重绘。协议不存在时只进入 `xdg_fallback`，不能 panic。

`xdg_fallback.rs` 创建 `app_id=trafficview` 的普通 `xdg_toplevel`，支持尺寸、透明内容、关闭和重连；明确不承诺贴边、置顶或任意定位。`trafficview overlay --session auto` 根据 `XDG_SESSION_TYPE`、`XDG_CURRENT_DESKTOP` 和协议枚举选择；`--session wayland` 强制只使用 Wayland，不回退 XWayland。

- [ ] **Step 4: 在 KDE 与 GNOME 验证**

Run: `cargo test -p trafficview-ui-wayland && cargo test --workspace`

KDE Plasma 6/Wayland Expected: 悬浮窗可贴边、拖拽吸附、穿透、多显示器和 150% 缩放文字清晰。GNOME/Wayland Expected: 日志明确“layer-shell unsupported; using xdg_toplevel”，显示普通窗口且不崩溃；顶栏常驻由 Task 4 验收。

- [ ] **Step 5: 提交**

```bash
git add Cargo.toml Cargo.lock crates/ui-wayland crates/cli/src/main.rs
git commit -m "feat: add Wayland overlay with compositor-safe fallback"
```

### Task 4: 交付 GNOME Shell 顶栏扩展

**Files:**
- Create: `frontends/gnome-extension/metadata.json`
- Create: `frontends/gnome-extension/extension.js`
- Create: `frontends/gnome-extension/stylesheet.css`

- [ ] **Step 1: 先定义可注入的代理测试和生命周期测试**

将 D-Bus 订阅与 UI 更新拆成 `createController(proxy, label)` 可测试函数。用 fake proxy 连续发出两个快照，断言 label 更新；发出非法/缺字段字典时保留上一有效值并记录一次警告；`disable()` 后 fake proxy 再发信号，断言 label 不再变化且 signal id 已断开。

- [ ] **Step 2: 运行 GJS 测试并确认失败**

Run: `gjs -m frontends/gnome-extension/tests/controller_test.js`

Expected: FAIL，controller 尚未实现。测试文件只创建 fake `St.Label`，不访问真实 session bus。

- [ ] **Step 3: 实现 ESM 扩展**

`metadata.json` 使用 UUID `trafficview@trafficview.local`，只列出实际完成测试的 GNOME shell-version。`extension.js` 遵循 45+ ESM 的 `import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js'`，在 `enable()`：

1. 建立 `Gio.DBusProxy`，名/路径/接口严格使用 D-Bus v1。
2. 先调用 `GetSnapshot`，再订阅 `Sample`，避免启动空窗和竞态。
3. 将上下行、CPU、内存格式化到 `St.Label`，加入 `Main.panel._rightBox`。
4. 点击使用 `PopupMenu` 提供暂停/恢复和打开设置命令，不直接读取 procfs。
5. name owner 消失时显示 `--` 并重连，owner 恢复后重新取快照。

`disable()` 必须断开信号、销毁 menu/label、清空代理引用；重复 enable/disable 不产生重复 indicator。

- [ ] **Step 4: 在真实 GNOME 验收**

安装到用户扩展目录后重新加载 GNOME Shell（Wayland 下注销再登录），启用扩展。

Expected: 顶栏每秒更新；点击菜单有效；禁用后无残留；daemon 停止时显示断开状态，恢复后自动更新；`journalctl /usr/bin/gnome-shell -f` 无本扩展异常。GNOME 无 AppIndicator 也能通过此扩展使用核心功能。

- [ ] **Step 5: 提交**

```bash
git add frontends/gnome-extension
git commit -m "feat: add GNOME Shell panel frontend"
```

### Task 5: 交付 Plasma widget 与 waybar/通用面板输出

**Files:**
- Create: `frontends/plasma-widget/metadata.json`
- Create: `frontends/plasma-widget/contents/ui/main.qml`
- Create: `frontends/plasma-widget/contents/ui/CompactRepresentation.qml`
- Create: `frontends/plasma-widget/contents/config/main.xml`
- Modify: `crates/cli/src/main.rs`
- Create: `crates/cli/tests/status_formats.rs`

- [ ] **Step 1: 先写 waybar JSON 失败测试**

```rust
#[test]
fn waybar_output_is_single_line_json_with_tooltip_and_state() {
    let output = format_waybar(&test_snapshot(), false).unwrap();
    assert!(!output.contains('\n'));
    let value: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["class"], "normal");
    assert!(value["text"].as_str().unwrap().contains("↓"));
    assert!(value["tooltip"].as_str().unwrap().contains("CPU"));
}

#[test]
fn disconnected_output_is_explicit() {
    let value: serde_json::Value = serde_json::from_str(&format_disconnected().unwrap()).unwrap();
    assert_eq!(value["class"], "disconnected");
    assert_eq!(value["text"], "TrafficView: --");
}
```

- [ ] **Step 2: 确认失败并实现格式出口**

Run: `cargo test -p trafficview-cli --test status_formats`

Expected: 先 FAIL。实现 `trafficview status --format json|waybar|polybar|tmux`，其中 `--json` 保持为 `--format json` 的兼容别名。每次调用只取一个 `GetSnapshot`，stdout 只输出一行；D-Bus 错误写到 stderr 并返回非零，waybar disconnected JSON 仅在显式 `--allow-disconnected` 时输出。

waybar 配置验收片段：

```json
"custom/trafficview": {
  "exec": "trafficview status --format waybar --allow-disconnected",
  "interval": 1,
  "return-type": "json"
}
```

- [ ] **Step 3: 实现 Plasma D-Bus consumer**

Plasmoid compact representation 显示上下行和 CPU，full representation 显示内存、网卡、时间戳与暂停按钮。使用 Plasma6 可用的 D-Bus QML 绑定直接连接 `com.trafficview.Daemon1`，禁止 `executable` 每秒启动 shell 作为正式数据路径；连接断开显示 `--`，恢复时重新调用 `GetSnapshot`。配置项只包括显示项和紧凑模式单位，真实采样配置仍走 daemon。

- [ ] **Step 4: 验证所有面板**

Run: `cargo test -p trafficview-cli --test status_formats && cargo test --workspace`

Expected: 自动测试 PASS。waybar 能解析每行 JSON，无多余日志；Plasma 6 可将 widget 加入 panel、注销重登后自动恢复，daemon 重启不产生重复实例；polybar/tmux 输出不含控制字符。

- [ ] **Step 5: 提交并关闭 M2**

```bash
git add frontends/plasma-widget crates/cli Cargo.lock
git commit -m "feat: integrate Plasma and scriptable status panels"
```

M2 完成条件：原生皮肤预览与 X11/Wayland 实际渲染一致，KDE/Wayland、GNOME/Wayland、waybar 三条路径均通过真实会话验收，且 M1 所有回归测试继续通过。
