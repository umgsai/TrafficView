//! Wayland 悬浮窗：优先 `zwlr_layer_surface_v1`，GNOME 等无 layer-shell 时降级为
//! 普通 `xdg_toplevel`。本机没有 Wayland 会话，因此这里只做编译期与纯几何验证。

pub mod layer_surface;
pub mod shm;
pub mod xdg_fallback;

use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use trafficview_core::{config::Config, ColorScheme, Snapshot};
use trafficview_render::{skin::Skin, AlertState, RenderedFrame, Renderer};

/// 快照、当前配色与告警；前端不需要各自订阅 portal 或修改渲染配置。
#[derive(Debug, Clone)]
pub(crate) struct SnapshotUpdate {
    pub snapshot: Snapshot,
    pub scheme: ColorScheme,
    pub alerts: AlertState,
    /// 托盘/设置窗口切换的 `overlay.visible`；Wayland 前端据此销毁或重建窗口。
    pub visible: bool,
}

/// 会话后端选择。GNOME 明确不支持 layer-shell，必须走普通窗口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    LayerShell,
    XdgFallback,
    X11,
}

impl Backend {
    pub fn choose(session_type: &str, desktop: &str, layer_shell_supported: bool) -> Self {
        if !session_type.eq_ignore_ascii_case("wayland") {
            return Self::X11;
        }
        let desktop = desktop.to_ascii_uppercase();
        if desktop.contains("GNOME") {
            return Self::XdgFallback;
        }
        if layer_shell_supported {
            Self::LayerShell
        } else {
            Self::XdgFallback
        }
    }
}

/// layer-shell 锚点，值与协议一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor(pub u32);

impl Anchor {
    pub const TOP: Self = Self(1);
    pub const BOTTOM: Self = Self(2);
    pub const LEFT: Self = Self(4);
    pub const RIGHT: Self = Self(8);
}

impl std::ops::BitOr for Anchor {
    type Output = Anchor;

    fn bitor(self, rhs: Self) -> Self::Output {
        Anchor(self.0 | rhs.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchored {
    pub anchor: Anchor,
    pub margin: (i32, i32),
}

/// 拖拽结束位置吸附到网格，并选择最近的边。
pub fn snap_anchor(position: [i32; 2], screen: [i32; 2], size: [i32; 2], grid: i32) -> Anchored {
    let grid = grid.max(1);
    let right = screen[0].saturating_sub(position[0].saturating_add(size[0]));
    let bottom = screen[1].saturating_sub(position[1].saturating_add(size[1]));

    let (horizontal, margin_x) = if right <= position[0] {
        (Anchor::RIGHT, snap(right, grid))
    } else {
        (Anchor::LEFT, snap(position[0], grid))
    };
    let (vertical, margin_y) = if bottom <= position[1] {
        (Anchor::BOTTOM, snap(bottom, grid))
    } else {
        (Anchor::TOP, snap(position[1], grid))
    };
    Anchored {
        anchor: horizontal | vertical,
        margin: (margin_x, margin_y),
    }
}

fn snap(value: i32, grid: i32) -> i32 {
    if value <= 0 {
        0
    } else {
        (((value as f64) / (grid as f64)).round() as i32).saturating_mul(grid)
    }
}

/// 逻辑尺寸 → 物理缓冲尺寸，向上取整保证文字不被裁切。
pub fn physical_size(logical: [u32; 2], scale: f64) -> [u32; 2] {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    [
        ((logical[0] as f64) * scale).round().max(1.0) as u32,
        ((logical[1] as f64) * scale).round().max(1.0) as u32,
    ]
}

/// 按显式后端运行。
pub fn run_with_backend(config: Config, backend: Backend, allow_fallback: bool) -> Result<()> {
    match backend {
        Backend::LayerShell => run(config, allow_fallback),
        Backend::XdgFallback => xdg_fallback::run(config),
        Backend::X11 => bail!("X11 backend must be started by trafficview-ui-x11"),
    }
}

/// 运行 Wayland 悬浮窗。`allow_fallback=false` 时禁止 layer-shell → xdg 降级。
pub fn run(config: Config, allow_fallback: bool) -> Result<()> {
    match layer_surface::run(config.clone()) {
        Ok(()) => Ok(()),
        Err(error) if allow_fallback && is_unsupported(&error) => {
            eprintln!("layer-shell unsupported; using xdg_toplevel");
            xdg_fallback::run(config)
        }
        Err(error) => Err(error),
    }
}

fn is_unsupported(error: &anyhow::Error) -> bool {
    format!("{error:#}").contains("layer-shell unsupported")
}

/// 固定布局或原生皮肤的统一帧来源。
pub(crate) enum FrameSource {
    Fixed(Box<Config>),
    Skin(Box<Renderer>),
}

impl FrameSource {
    pub(crate) fn from_config(config: &Config) -> Result<Self> {
        let path = config.overlay.skin.trim();
        if path.is_empty() {
            return Ok(Self::Fixed(Box::new(config.clone())));
        }
        let skin = Skin::load(path).with_context(|| format!("cannot load skin {path}"))?;
        let renderer = Renderer::new(skin).context("cannot prepare skin renderer")?;
        Ok(Self::Skin(Box::new(renderer)))
    }

    /// 逻辑画布尺寸，用于计算锚点与边距。
    pub(crate) fn logical_size(&self) -> [u32; 2] {
        match self {
            Self::Skin(renderer) => {
                let canvas = &renderer.skin().canvas;
                [canvas.width, canvas.height]
            }
            Self::Fixed(config) => {
                let rows = config
                    .overlay
                    .items
                    .iter()
                    .filter(|item| item.visible)
                    .count() as u32;
                [220, 16 + rows * 24]
            }
        }
    }

    pub(crate) fn render(
        &mut self,
        snapshot: &Snapshot,
        scale: f64,
        scheme: ColorScheme,
        alerts: &AlertState,
    ) -> Result<RenderedFrame> {
        match self {
            Self::Fixed(config) => Ok(trafficview_render::render_with_scheme_and_alerts(
                snapshot,
                config,
                (96.0 * scale).round().max(1.0) as u32,
                scheme,
                alerts,
            )),
            Self::Skin(renderer) => {
                renderer.set_color_scheme(scheme);
                renderer
                    .render_with_alerts(snapshot, scale as f32, alerts)
                    .map_err(|error| anyhow::anyhow!(error))
            }
        }
    }
}

/// D-Bus 快照线程：先 `GetSnapshot` 再订阅 `Sample`，daemon 重启后自动重连。
pub(crate) fn spawn_snapshot_stream() -> Receiver<SnapshotUpdate> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("wayland overlay: cannot start runtime: {error}");
                return;
            }
        };
        loop {
            if let Err(error) = runtime.block_on(stream_once(&sender)) {
                eprintln!("wayland overlay: {error:#}");
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
    receiver
}

async fn stream_once(sender: &std::sync::mpsc::Sender<SnapshotUpdate>) -> Result<()> {
    let connection = zbus::Connection::session()
        .await
        .context("cannot connect to the session bus")?;
    let proxy: zbus::Proxy<'_> = zbus::ProxyBuilder::new(&connection)
        .destination("com.trafficview.Daemon1")?
        .path("/com/trafficview/Daemon1")?
        .interface("com.trafficview.Daemon1")?
        // 关闭属性缓存：托盘切换 OverlayVisible 后必须读到新值，
        // 默认的 Lazily 缓存会一直返回首次读取的结果。
        .cache_properties(zbus::CacheProperties::No)
        .build()
        .await
        .context("cannot build the trafficviewd proxy")?;
    let mut scheme = read_scheme(&proxy).await;
    let mut alerts = AlertState::default();
    let mut visible = read_overlay_visible(&proxy).await;
    let mut last: Option<Snapshot> = None;
    if let Ok(reply) = proxy.call_method("GetSnapshot", &()).await {
        let dict: std::collections::HashMap<String, zbus::zvariant::OwnedValue> =
            reply.body().deserialize()?;
        let snapshot = trafficview_daemon::dbus::snapshot_from_dbus(&dict)?;
        last = Some(snapshot.clone());
        if sender
            .send(SnapshotUpdate {
                snapshot,
                scheme,
                alerts: alerts.clone(),
                visible,
            })
            .is_err()
        {
            bail!("overlay receiver closed");
        }
    }
    let mut samples = proxy
        .receive_signal("Sample")
        .await
        .context("cannot subscribe to Sample")?;
    let mut configs = proxy
        .receive_signal("ConfigChanged")
        .await
        .context("cannot subscribe to ConfigChanged")?;
    let mut alert_signals = proxy
        .receive_signal("Alert")
        .await
        .context("cannot subscribe to Alert")?;
    loop {
        tokio::select! {
            message = samples.next() => {
                let Some(message) = message else { break };
                let dict: std::collections::HashMap<String, zbus::zvariant::OwnedValue> =
                    message.body().deserialize()?;
                let snapshot = trafficview_daemon::dbus::snapshot_from_dbus(&dict)?;
                last = Some(snapshot.clone());
                // 每个采样周期都复核一次可见性：即使错过一次 ConfigChanged
                // （重连、daemon 重启），托盘开关也能在一个采样周期内生效。
                visible = read_overlay_visible(&proxy).await;
                if sender
                    .send(SnapshotUpdate {
                        snapshot,
                        scheme,
                        alerts: alerts.clone(),
                        visible,
                    })
                    .is_err()
                {
                    break;
                }
            }
            message = configs.next() => {
                if message.is_none() {
                    break;
                }
                scheme = read_scheme(&proxy).await;
                visible = read_overlay_visible(&proxy).await;
                if let Some(snapshot) = &last {
                    if sender
                        .send(SnapshotUpdate {
                            snapshot: snapshot.clone(),
                            scheme,
                            alerts: alerts.clone(),
                            visible,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
            message = alert_signals.next() => {
                let Some(message) = message else { break };
                let (level, code, text): (String, String, String) = message.body().deserialize()?;
                if alerts.apply_event(&level, &code, &text) {
                    if let Some(snapshot) = &last {
                        if sender
                            .send(SnapshotUpdate {
                                snapshot: snapshot.clone(),
                                scheme,
                                alerts: alerts.clone(),
                                visible,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

async fn read_scheme(proxy: &zbus::Proxy<'_>) -> ColorScheme {
    match proxy.get_property::<String>("ColorScheme").await {
        Ok(value) if value.eq_ignore_ascii_case("light") => ColorScheme::Light,
        _ => ColorScheme::Dark,
    }
}

/// 托盘「显示/隐藏悬浮窗」写的是 daemon 的 `OverlayVisible` 属性。
/// 读取失败时保持显示，避免因一次 D-Bus 抖动让悬浮窗永久消失。
async fn read_overlay_visible(proxy: &zbus::Proxy<'_>) -> bool {
    proxy.get_property::<bool>("OverlayVisible").await.unwrap_or(true)
}
