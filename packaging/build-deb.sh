#!/usr/bin/env bash
# 构建 TrafficView 的 Debian 包（单个主包）。
#
# 不包含 eBPF/AF_PACKET 特权 helper 及其 system 单元、D-Bus 策略：
# process_traffic.backend 选到这些后端时由 daemon 按“不可用”降级。
#
# 产物：dist/trafficview_<version>-<revision>_<arch>.deb 与 dist/SHA256SUMS
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

REVISION=1
MAINTAINER="TrafficView Maintainers <maintainers@trafficview.invalid>"
SETTINGS_DIR="frontends/settings"
SETTINGS_BIN="$SETTINGS_DIR/target/release/trafficview-settings"
STAGE="target/deb-stage"
SHLIB_DIR="target/deb-shlibdeps"

VERSION=$(cargo metadata --no-deps --format-version 1 |
    python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "trafficview-cli"))')
ARCH=$(dpkg --print-architecture)
ARTIFACT="dist/trafficview_${VERSION}-${REVISION}_${ARCH}.deb"

# 1. 质量门（设置界面是独立 workspace，fmt/clippy/test 一并覆盖）。
if [[ "${SKIP_QUALITY_GATE:-0}" != "1" ]]; then
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    # daemon 集成测试抢占 com.trafficview.Daemon1，需要独立会话总线。
    dbus-run-session -- cargo test --workspace
    cargo fmt --manifest-path "$SETTINGS_DIR/Cargo.toml" -- --check
    cargo clippy --manifest-path "$SETTINGS_DIR/Cargo.toml" --all-targets -- -D warnings
    cargo test --manifest-path "$SETTINGS_DIR/Cargo.toml"
fi

# 2. release 构建（根 workspace 与独立的设置界面各构建一次）。
cargo build --workspace --release
cargo build --manifest-path "$SETTINGS_DIR/Cargo.toml" --release

for bin in target/release/trafficview target/release/trafficviewd "$SETTINGS_BIN"; do
    [[ -s "$bin" ]] || {
        echo "缺少 release 产物：$bin" >&2
        exit 1
    }
done

# 3. 组装文件树。
rm -rf "$STAGE"
install -Dm755 target/release/trafficview "$STAGE/usr/bin/trafficview"
install -Dm755 target/release/trafficviewd "$STAGE/usr/bin/trafficviewd"
install -Dm755 "$SETTINGS_BIN" "$STAGE/usr/bin/trafficview-settings"
install -Dm755 packaging/libexec/trafficview-session \
    "$STAGE/usr/libexec/trafficview-session"
install -Dm644 packaging/systemd/trafficviewd.service \
    "$STAGE/usr/lib/systemd/user/trafficviewd.service"
install -Dm644 packaging/applications/trafficview.desktop \
    "$STAGE/usr/share/applications/trafficview.desktop"
install -Dm644 packaging/applications/trafficview-settings.desktop \
    "$STAGE/usr/share/applications/trafficview-settings.desktop"
for entry in packaging/autostart/*.desktop; do
    install -Dm644 "$entry" "$STAGE/etc/xdg/autostart/$(basename "$entry")"
done

install -Dm644 packaging/icons/trafficview.svg \
    "$STAGE/usr/share/icons/hicolor/scalable/apps/trafficview.svg"
for size in 16 32 48 64 128 256; do
    install -Dm644 "packaging/icons/hicolor/${size}x${size}/apps/trafficview.png" \
        "$STAGE/usr/share/icons/hicolor/${size}x${size}/apps/trafficview.png"
done

install -Dm644 packaging/copyright "$STAGE/usr/share/doc/trafficview/copyright"
install -Dm644 README.md "$STAGE/usr/share/doc/trafficview/README.md"
install -Dm644 docs/dbus-v1.md "$STAGE/usr/share/doc/trafficview/dbus-v1.md"
gzip -9nc packaging/changelog >"$STAGE/usr/share/doc/trafficview/changelog.gz"
chmod 644 "$STAGE/usr/share/doc/trafficview/changelog.gz"

# /etc 下的自启动条目按配置文件处理，升级时保留用户修改。
mkdir -p "$STAGE/DEBIAN"
find "$STAGE/etc" -type f -printf '%P\n' | sed 's|^|/etc/|' >"$STAGE/DEBIAN/conffiles"

# 4. 由 dpkg-shlibdeps 推导动态库依赖（GTK4/libadwaita 等）。
rm -rf "$SHLIB_DIR"
mkdir -p "$SHLIB_DIR/debian"
cat >"$SHLIB_DIR/debian/control" <<EOF
Source: trafficview
Section: utils
Priority: optional
Maintainer: $MAINTAINER
Standards-Version: 4.6.2

Package: trafficview
Architecture: any
Description: placeholder
EOF
DEPENDS=$(cd "$SHLIB_DIR" && dpkg-shlibdeps -O \
    "$ROOT/$STAGE/usr/bin/trafficview" \
    "$ROOT/$STAGE/usr/bin/trafficviewd" \
    "$ROOT/$STAGE/usr/bin/trafficview-settings" | sed -n 's/^shlibs:Depends=//p')
[[ -n "$DEPENDS" ]] || {
    echo "dpkg-shlibdeps 未推导出任何依赖" >&2
    exit 1
}

INSTALLED_SIZE=$(du -sk --exclude=DEBIAN "$STAGE" | cut -f1)

cat >"$STAGE/DEBIAN/control" <<EOF
Package: trafficview
Version: ${VERSION}-${REVISION}
Architecture: ${ARCH}
Maintainer: ${MAINTAINER}
Installed-Size: ${INSTALLED_SIZE}
Depends: ${DEPENDS}
Section: utils
Priority: optional
Description: real-time network traffic and system load monitor
 TrafficView samples /proc, sysfs and netlink in a small D-Bus daemon and
 renders an X11 floating overlay, a StatusNotifierItem tray icon and a
 GTK4 settings window. This package does not include the privileged
 eBPF/AF_PACKET helpers; those backends degrade to unavailable.
EOF

# 5. 校验和清单与构建。
(
    cd "$STAGE"
    find . -type f ! -path './DEBIAN/*' -printf '%P\0' | sort -z |
        xargs -0 md5sum >DEBIAN/md5sums
)

mkdir -p dist
dpkg-deb --root-owner-group -Zxz --build "$STAGE" "$ARTIFACT"
sha256sum "$ARTIFACT" >dist/SHA256SUMS

echo
dpkg-deb -I "$ARTIFACT"
echo
sha256sum -c dist/SHA256SUMS
