#!/usr/bin/env bash
# 发布构建：先跑完整质量门，再产出二进制与校验和。
# 设置前端是独立 workspace（GTK4 依赖不在根构建中），由打包步骤单独构建。
set -euo pipefail

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
# 守护进程的集成测试会抢占 com.trafficview.Daemon1，必须在独立会话总线上运行，
# 否则与正在运行的 trafficviewd 冲突。
dbus-run-session -- cargo test --workspace
cargo build --workspace --release

mkdir -p dist
cp target/release/trafficview target/release/trafficviewd dist/
sha256sum dist/trafficview dist/trafficviewd > dist/SHA256SUMS
