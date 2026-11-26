# 设置界面「关于」页面设计

日期：2026-11-26
状态：已确认，待实现

## 目标

在设置界面侧边栏增加第七项「关于」，页面内展示应用图标、名称、版本号、
功能介绍与 GitHub 项目链接。仅涉及 `frontends/settings`，不改 D-Bus 契约。

## 现状

`frontends/settings/src/window.rs` 使用 `gtk::Stack` + `gtk::StackSidebar`，
六个页面（通用/网络/显示/告警/历史/插件与高级）均通过
`stack.add_titled(&page_scroll(&page), Some(id), "标题")` 注册，页面为
`adw::PreferencesPage`，内部分组用 `adw::PreferencesGroup`。

另发现 `frontends/settings/Cargo.toml` 的 `version` 仍为 `1.0.2`，与主包
1.1.0 不一致；关于页要显示版本号，需一并同步。

## 页面内容

在六页之后追加：

```rust
stack.add_titled(&page_scroll(&about), Some("about"), "关于");
```

`about` 为 `adw::PreferencesPage`，`set_title("关于")`，图标
`help-about-symbolic`。内容自上而下：

1. **头部**：`gtk::Image::from_icon_name("trafficview")`（图标已安装到
   hicolor 主题，像素尺寸 64）；标题 `TrafficView`；副标题
   `版本 {env!("CARGO_PKG_VERSION")}`。
2. **「功能介绍」组**：一段自动换行的 `gtk::Label`，文案：
   "Linux 桌面实时流量与系统负载监控。悬浮窗显示上下行速率、CPU、内存、
   GPU 及温度；支持托盘控制、SQLite 历史统计、阈值告警与 waybar 等多前端输出。
   所有数据由守护进程采集并经 D-Bus 分发。"
3. **「项目链接」组**：`gtk::LinkButton` 指向
   `https://github.com/umgsai/TrafficView`，文字 "GitHub 项目主页"，
   点击由系统默认浏览器打开。

## 版本号同步

`frontends/settings/Cargo.toml` 的 `version` 改为 `1.1.0`，与主包一致；
关于页通过 `env!("CARGO_PKG_VERSION")` 显示，避免硬编码。

## 错误处理

静态页面，无运行时错误分支；链接打开行为由 GTK/系统浏览器负责。

## 测试

- `cargo fmt --manifest-path frontends/settings/Cargo.toml -- --check`
- `cargo clippy --manifest-path frontends/settings/Cargo.toml --all-targets -- -D warnings`
- `cargo test --manifest-path frontends/settings/Cargo.toml`（既有测试保持通过）
- 人工：启动设置界面，侧边栏出现「关于」，版本显示 1.1.0，链接可点击。

## 非目标

- 不引入 `AdwAboutWindow` 弹窗、不加许可证/致谢等额外页面内容。
- 不改其它 crate、不升主版本号、不处理 deb 拆分。
