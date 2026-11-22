//! TrafficView 独立 GTK4 设置界面入口。
//!
//! 本 crate 是独立 workspace（见同目录 Cargo.toml 的 `[workspace]`），
//! 根 workspace 通过 `exclude` 排除本目录，因此默认构建不会解析 GTK 依赖。

mod autostart;
mod config_model;
mod dbus;
mod window;

fn main() {
    window::run();
}
