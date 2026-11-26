//! X11 ARGB override-redirect 悬浮窗：只消费 D-Bus 快照，不读 /proc。

use std::{
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use trafficview_core::{
    config::{Config, ConfigStore, PositionPreset},
    Snapshot,
};
use trafficview_render::RenderedFrame;
use x11rb::{
    connection::{Connection as _, RequestConnection as _},
    protocol::{
        shape::{ConnectionExt as _, SK, SO},
        xproto::{
            AtomEnum, ClipOrdering, ColormapAlloc, ConnectionExt as _, CreateGCAux,
            CreateWindowAux, EventMask, ImageFormat, PropMode, WindowClass,
        },
        Event,
    },
};

#[zbus::proxy(
    interface = "com.trafficview.Daemon1",
    default_service = "com.trafficview.Daemon1",
    default_path = "/com/trafficview/Daemon1"
)]
trait Daemon1 {
    fn get_snapshot(
        &self,
    ) -> zbus::Result<std::collections::HashMap<String, zbus::zvariant::OwnedValue>>;
    fn update_config(
        &self,
        updates: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::Result<()>;
    #[zbus(property)]
    fn color_scheme(&self) -> zbus::Result<String>;
    #[zbus(signal)]
    fn sample(
        &self,
        snapshot: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::Result<()>;
    #[zbus(signal)]
    fn config_changed(&self) -> zbus::Result<()>;
    #[zbus(signal)]
    fn alert(&self, level: String, code: String, message: String) -> zbus::Result<()>;
}

fn parse_scheme(value: &str) -> trafficview_core::ColorScheme {
    if value.eq_ignore_ascii_case("light") {
        trafficview_core::ColorScheme::Light
    } else {
        trafficview_core::ColorScheme::Dark
    }
}

pub struct FrameUpdate {
    pub frame: RenderedFrame,
    pub click_through: bool,
    /// 配置中的悬浮窗可见性；隐藏时不向窗口提交像素。
    pub visible: bool,
    /// 位置预设；根据屏幕尺寸解析实际坐标。
    pub position_preset: PositionPreset,
    /// Manual 模式下的坐标；其他 preset 会被忽略。
    pub manual_position: [i32; 2],
    /// 是否设置 _NET_WM_STATE_ABOVE。
    pub always_on_top: bool,
}

/// 将窗口位置夹回屏幕内，避免拖出可见区域。
pub fn clamp_position(position: (i32, i32), size: (u32, u32), screen: (u16, u16)) -> (i32, i32) {
    let max_x = screen.0 as i32 - size.0 as i32;
    let max_y = screen.1 as i32 - size.1 as i32;
    (
        position.0.clamp(0, max_x.max(0)),
        position.1.clamp(0, max_y.max(0)),
    )
}

/// 根据位置预设解析实际坐标；Manual 时直接用 config.position。
pub fn resolve_position(
    preset: PositionPreset,
    manual: [i32; 2],
    size: (u32, u32),
    screen: (u16, u16),
) -> (i32, i32) {
    let margin = 200i32;
    let (sw, sh) = (screen.0 as i32, screen.1 as i32);
    let (w, h) = (size.0 as i32, size.1 as i32);
    let pos = match preset {
        PositionPreset::BottomRight => (sw - w - margin, sh - h - margin),
        PositionPreset::BottomLeft => (margin, sh - h - margin),
        PositionPreset::TopRight => (sw - w - margin, margin),
        PositionPreset::TopLeft => (margin, margin),
        PositionPreset::Manual => (manual[0], manual[1]),
    };
    clamp_position(pos, size, screen)
}

/// 启动悬浮窗；阻塞直到窗口线程退出或 D-Bus 断开。
pub fn run(config: Config) -> Result<()> {
    if std::env::var_os("DISPLAY").is_none() {
        bail!("DISPLAY is not set; the X11 overlay requires a graphical session");
    }
    let dpi = query_dpi().unwrap_or(96);
    eprintln!(
        "overlay starting on {} at {dpi} DPI",
        std::env::var("DISPLAY").unwrap_or_default()
    );
    let (frame_tx, frame_rx) = mpsc::channel::<FrameUpdate>();
    // 拖动结束后由 X11 线程把坐标交给 D-Bus 线程，经 UpdateConfig 全链路同步并持久化。
    let (move_tx, move_rx) = tokio::sync::mpsc::unbounded_channel::<[i32; 2]>();
    let store = ConfigStore::xdg().ok();
    let initial = config.clone();
    let x11_store = store.clone();
    let x11_thread = thread::spawn(move || x11_loop(initial, frame_rx, x11_store, move_tx));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("cannot start the overlay runtime")?;
    let result = runtime.block_on(dbus_loop(config, dpi, frame_tx, store, move_rx));
    if let Err(error) = &result {
        eprintln!("overlay: {error:#}");
    }
    let _ = x11_thread.join();
    result
}

async fn dbus_loop(
    mut config: Config,
    dpi: u32,
    frames: Sender<FrameUpdate>,
    store: Option<ConfigStore>,
    mut moves: tokio::sync::mpsc::UnboundedReceiver<[i32; 2]>,
) -> Result<()> {
    // 皮肤路径无效时降级为默认皮肤并保留说明，不能因此让整个悬浮窗退出。
    let mut skin_renderer = match load_skin_renderer(&config) {
        Ok(renderer) => renderer,
        Err(error) => {
            eprintln!(
                "overlay: cannot load the configured skin, using the default skin: {error:#}"
            );
            None
        }
    };
    let connection = zbus::Connection::session()
        .await
        .context("cannot connect to the session bus; is trafficviewd running?")?;
    // 固定名做单实例：菜单入口与会话自启动都可能在同一个会话里拉起悬浮窗。
    match connection.request_name("com.trafficview.Overlay").await {
        Ok(()) => {}
        Err(zbus::Error::NameTaken) => {
            eprintln!("overlay: another instance is already running");
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    }
    let proxy = Daemon1Proxy::new(&connection)
        .await
        .context("cannot reach trafficviewd; start the daemon first")?;
    let mut scheme = proxy
        .color_scheme()
        .await
        .map(|value| parse_scheme(&value))
        .unwrap_or_default();
    if let Some(renderer) = skin_renderer.as_mut() {
        renderer.set_color_scheme(scheme);
    }
    let mut stream = proxy
        .receive_sample()
        .await
        .context("cannot subscribe to the Sample signal")?;
    let mut config_stream = proxy
        .receive_config_changed()
        .await
        .context("cannot subscribe to the ConfigChanged signal")?;
    let mut alert_stream = proxy
        .receive_alert()
        .await
        .context("cannot subscribe to the Alert signal")?;
    let mut alerts = trafficview_render::AlertState::default();
    let mut last_snapshot: Option<Snapshot> = None;
    // daemon 退出时 Sample 流不会自动结束，需要主动探测服务名是否仍被持有；
    // 否则 daemon/托盘退出后悬浮窗进程与窗口会一直残留。
    let bus = zbus::fdo::DBusProxy::new(&connection)
        .await
        .context("cannot query the session bus")?;
    let mut health = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            signal = stream.next() => {
                let Some(signal) = signal else { break };
                let args = signal.args()?;
                let snapshot = trafficview_daemon::dbus::snapshot_from_dbus(&args.snapshot)?;
                send_frame(&mut skin_renderer, &config, dpi, scheme, &snapshot, &alerts, &frames)?;
                last_snapshot = Some(snapshot);
            }
            changed = config_stream.next() => {
                if changed.is_none() {
                    break;
                }
                // ConfigChanged 是托盘/设置窗口改写显示配置的入口：
                // 必须重新加载，否则可见性、位置、透明度与皮肤都不会生效。
                if let Some(store) = &store {
                    if let Ok(next) = store.load() {
                        if next.overlay.skin != config.overlay.skin {
                            match load_skin_renderer(&next) {
                                Ok(renderer) => skin_renderer = renderer,
                                Err(error) => eprintln!(
                                    "overlay: cannot load skin {}, keeping the current skin: {error:#}",
                                    next.overlay.skin
                                ),
                            }
                        }
                        config = next;
                    }
                }
                if let Ok(value) = proxy.color_scheme().await {
                    scheme = parse_scheme(&value);
                }
                // 主题或显示配置变化时即使采样文本没变也要重绘；
                // 暂停时没有新帧，用空快照保证可见性切换仍能送达窗口线程。
                let snapshot = last_snapshot.clone().unwrap_or_else(empty_snapshot);
                send_frame(&mut skin_renderer, &config, dpi, scheme, &snapshot, &alerts, &frames)?;
            }
            signal = alert_stream.next() => {
                let Some(signal) = signal else { break };
                let args = signal.args()?;
                if alerts.apply_event(&args.level, &args.code, &args.message) {
                    if let Some(snapshot) = &last_snapshot {
                        send_frame(&mut skin_renderer, &config, dpi, scheme, snapshot, &alerts, &frames)?;
                    }
                }
            }
            position = moves.recv() => {
                let Some(position) = position else { break };
                if let Err(error) = persist_position(&proxy, position).await {
                    eprintln!("overlay: cannot persist the dragged position: {error:#}");
                }
            }
            _ = health.tick() => {
                let owned = match zbus::names::BusName::try_from("com.trafficview.Daemon1") {
                    Ok(name) => bus.name_has_owner(name).await.unwrap_or(false),
                    Err(_) => false,
                };
                if !owned {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// 拖动结束后经 daemon 写盘并广播 ConfigChanged：daemon 的内存配置同步更新，
/// 否则后续任何配置操作都会用旧坐标覆盖掉用户拖动的位置。
async fn persist_position(proxy: &Daemon1Proxy<'_>, position: [i32; 2]) -> Result<()> {
    use std::collections::HashMap;
    use zbus::zvariant::{OwnedValue, Value};

    let mut overlay: HashMap<String, Value<'static>> = HashMap::new();
    overlay.insert("position".into(), Value::from(position.to_vec()));
    overlay.insert("position_preset".into(), Value::from("manual"));
    let mut updates: HashMap<String, OwnedValue> = HashMap::new();
    updates.insert(
        "overlay".into(),
        OwnedValue::try_from(Value::from(overlay)).map_err(|error| anyhow::anyhow!(error))?,
    );
    proxy.update_config(updates).await?;
    Ok(())
}

fn send_frame(
    skin_renderer: &mut Option<trafficview_render::Renderer>,
    config: &Config,
    dpi: u32,
    scheme: trafficview_core::ColorScheme,
    snapshot: &Snapshot,
    alerts: &trafficview_render::AlertState,
    frames: &Sender<FrameUpdate>,
) -> Result<()> {
    let frame = match skin_renderer {
        Some(renderer) => {
            renderer.set_color_scheme(scheme);
            renderer
                .render_with_alerts(
                    snapshot,
                    dpi as f32 / trafficview_render::BASE_DPI as f32,
                    alerts,
                )
                .map_err(|error| anyhow::anyhow!(error))?
        }
        None => {
            trafficview_render::render_with_scheme_and_alerts(snapshot, config, dpi, scheme, alerts)
        }
    };
    frames
        .send(FrameUpdate {
            frame,
            click_through: config.overlay.click_through,
            visible: config.overlay.visible,
            position_preset: config.overlay.position_preset,
            manual_position: config.overlay.position,
            always_on_top: config.overlay.always_on_top,
        })
        .map_err(|_| anyhow::anyhow!("overlay window closed"))
}

fn load_skin_renderer(config: &Config) -> Result<Option<trafficview_render::Renderer>> {
    let path = config.overlay.skin.trim();
    if path.is_empty() {
        return Ok(None);
    }
    let skin = trafficview_render::skin::Skin::load(path)
        .map_err(|error| anyhow::anyhow!(error))
        .with_context(|| format!("cannot load skin {path}"))?;
    let renderer = trafficview_render::Renderer::new(skin)
        .map_err(|error| anyhow::anyhow!(error))
        .context("cannot prepare skin renderer")?;
    Ok(Some(renderer))
}

fn query_dpi() -> Option<u32> {
    let (connection, screen_num) = x11rb::connect(None).ok()?;
    let screen = connection.setup().roots.get(screen_num)?;
    let reply = connection
        .get_property(
            false,
            screen.root,
            AtomEnum::RESOURCE_MANAGER,
            AtomEnum::STRING,
            0,
            1024,
        )
        .ok()?
        .reply()
        .ok()?;
    let text = String::from_utf8_lossy(&reply.value);
    text.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim() == "Xft.dpi" {
            value.trim().parse::<u32>().ok()
        } else {
            None
        }
    })
}

struct WindowContext {
    connection: x11rb::rust_connection::RustConnection,
    screen_num: usize,
    window: u32,
    gc: u32,
    depth: u8,
    position: (i32, i32),
    visible: bool,
    size: (u32, u32),
    last_key: String,
    dragging: Option<(i16, i16, i32, i32)>,
    /// 拖动结束后的坐标；在 daemon 回传 Manual 配置前生效，避免被旧预设弹回。
    pending_manual: Option<[i32; 2]>,
    /// 把拖动结果交给 D-Bus 线程持久化。
    moves: tokio::sync::mpsc::UnboundedSender<[i32; 2]>,
    /// 是否设置 _NET_WM_STATE_ABOVE。
    always_on_top: bool,
}

impl WindowContext {
    fn new(config: &Config, moves: tokio::sync::mpsc::UnboundedSender<[i32; 2]>) -> Result<Self> {
        let (connection, screen_num) = x11rb::connect(None)?;
        let screen = &connection.setup().roots[screen_num];
        let visual = screen
            .allowed_depths
            .iter()
            .find(|depth| depth.depth == 32)
            .and_then(|depth| depth.visuals.first());
        let (depth, visual_id, cmap) = match visual {
            Some(visual) => {
                let cmap = connection.generate_id()?;
                connection.create_colormap(
                    ColormapAlloc::NONE,
                    cmap,
                    screen.root,
                    visual.visual_id,
                )?;
                (32u8, visual.visual_id, cmap)
            }
            None => (
                screen.root_depth,
                screen.root_visual,
                screen.default_colormap,
            ),
        };
        let always_on_top = config.overlay.always_on_top;
        let overlay_size = {
            let layout = trafficview_render::layout(&config.overlay.items, &sizing_snapshot(), 96);
            (layout.width, layout.height)
        };
        let position = resolve_position(
            config.overlay.position_preset,
            config.overlay.position,
            overlay_size,
            (screen.width_in_pixels, screen.height_in_pixels),
        );
        let width = overlay_size.0 as u16;
        let height = overlay_size.1 as u16;
        // overlay 永远是悬浮窗（override_redirect + DOCK 类型），
        // 不受 show_in_taskbar 影响——后者只控制 tray 图标旁是否显示实时指标。
        let aux = CreateWindowAux::new()
            .background_pixel(0)
            .border_pixel(0)
            .colormap(cmap)
            .override_redirect(1)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::STRUCTURE_NOTIFY,
            );
        let window = connection.generate_id()?;
        connection.create_window(
            depth,
            window,
            screen.root,
            position.0 as i16,
            position.1 as i16,
            width,
            height,
            0,
            WindowClass::INPUT_OUTPUT,
            visual_id,
            &aux,
        )?;
        let gc = connection.generate_id()?;
        connection.create_gc(gc, window, &CreateGCAux::new())?;

        // overlay 永远是 DOCK 类型（不受 WM 管理），不进入任务栏。
        let wm_type_atom = intern(&connection, b"_NET_WM_WINDOW_TYPE")?;
        let type_atom = intern(&connection, b"_NET_WM_WINDOW_TYPE_DOCK")?;
        connection.change_property(
            PropMode::REPLACE,
            window,
            wm_type_atom,
            AtomEnum::ATOM,
            32,
            1,
            &type_atom.to_ne_bytes(),
        )?;

        if always_on_top {
            let above = intern(&connection, b"_NET_WM_STATE_ABOVE")?;
            let state = intern(&connection, b"_NET_WM_STATE")?;
            connection.change_property(
                PropMode::REPLACE,
                window,
                state,
                AtomEnum::ATOM,
                32,
                1,
                &above.to_ne_bytes(),
            )?;
        }
        let class = intern(&connection, b"WM_CLASS")?;
        connection.change_property(
            PropMode::REPLACE,
            window,
            class,
            AtomEnum::STRING,
            8,
            b"trafficview\0TrafficView\0".len() as u32,
            b"trafficview\0TrafficView\0",
        )?;
        // 配置为隐藏时不映射窗口；之后可通过 ConfigChanged 显示。
        if config.overlay.visible {
            connection.map_window(window)?;
        }
        connection.flush()?;
        let attributes = connection.get_window_attributes(window)?.reply();
        attributes.context("the X server rejected the overlay window")?;
        Ok(Self {
            connection,
            screen_num,
            window,
            gc,
            depth,
            position,
            visible: config.overlay.visible,
            size: (width as u32, height as u32),
            last_key: String::new(),
            dragging: None,
            pending_manual: None,
            moves,
            always_on_top,
        })
    }

    fn draw(&mut self, update: &FrameUpdate) -> Result<()> {
        // always_on_top 变化需要重建窗口（WM state 在已映射窗口上难以可靠修改）。
        // show_in_taskbar 只影响 tray title，与 overlay 窗口行为无关，不触发重建。
        let mode_changed = update.always_on_top != self.always_on_top;
        if mode_changed {
            return Err(anyhow::anyhow!("window mode changed; recreate needed"));
        }
        self.apply_position(update.position_preset, update.manual_position)?;
        let became_visible = update.visible && !self.visible;
        if became_visible {
            // 重新映射后窗口内容可能已丢失，强制重画一次。
            self.last_key.clear();
        }
        self.set_visible(update.visible)?;
        if !update.visible {
            self.connection.flush()?;
            return Ok(());
        }
        let frame = &update.frame;
        if (frame.width as u16, frame.height as u16) != (self.size.0 as u16, self.size.1 as u16) {
            self.size = (frame.width, frame.height);
            self.connection.configure_window(
                self.window,
                &x11rb::protocol::xproto::ConfigureWindowAux::new()
                    .width(frame.width)
                    .height(frame.height),
            )?;
        }
        self.apply_click_through(update.click_through)?;
        if frame.cache_key == self.last_key {
            // 内容没变时仍要提交位置/穿透相关的请求。
            self.connection.flush()?;
            return Ok(());
        }
        // 大帧分块提交，避免超过 X server 的最大请求长度。
        let max_bytes = self.connection.maximum_request_bytes();
        let stride = frame.width as usize * 4;
        let rows_per_chunk = ((max_bytes.saturating_sub(1024)) / stride.max(1)).max(1);
        let mut row = 0usize;
        while row < frame.height as usize {
            let rows = rows_per_chunk.min(frame.height as usize - row);
            let start = row * stride;
            let end = start + rows * stride;
            self.connection.put_image(
                ImageFormat::Z_PIXMAP,
                self.window,
                self.gc,
                frame.width as u16,
                rows as u16,
                0,
                row as i16,
                0,
                self.depth,
                &frame.argb[start..end],
            )?;
            row += rows;
        }
        self.last_key = frame.cache_key.clone();
        self.connection.flush()?;
        Ok(())
    }

    fn set_visible(&mut self, visible: bool) -> Result<()> {
        if visible == self.visible {
            return Ok(());
        }
        if visible {
            self.connection.map_window(self.window)?;
        } else {
            self.connection.unmap_window(self.window)?;
        }
        self.visible = visible;
        Ok(())
    }

    /// 根据位置预设解析目标坐标并移动窗口；拖动中不打断，让事件循环维持位置。
    fn apply_position(&mut self, preset: PositionPreset, manual: [i32; 2]) -> Result<()> {
        if self.dragging.is_some() {
            return Ok(());
        }
        // 拖动结果要经 D-Bus 往返才会出现在后续帧里；在此之前忽略仍带着旧预设的帧。
        if let Some(pending) = self.pending_manual {
            if preset == PositionPreset::Manual && manual == pending {
                self.pending_manual = None;
            } else {
                return Ok(());
            }
        }
        let (screen_w, screen_h) = {
            let screen = &self.connection.setup().roots[self.screen_num];
            (screen.width_in_pixels, screen.height_in_pixels)
        };
        let target = resolve_position(preset, manual, self.size, (screen_w, screen_h));
        if target != self.position {
            self.position = target;
            self.connection.configure_window(
                self.window,
                &x11rb::protocol::xproto::ConfigureWindowAux::new()
                    .x(target.0)
                    .y(target.1),
            )?;
        }
        Ok(())
    }

    fn apply_click_through(&self, click_through: bool) -> Result<()> {
        if click_through {
            self.connection.shape_rectangles(
                SO::SET,
                SK::INPUT,
                ClipOrdering::UNSORTED,
                self.window,
                0,
                0,
                &[],
            )?;
        } else {
            self.connection.shape_rectangles(
                SO::SET,
                SK::INPUT,
                ClipOrdering::UNSORTED,
                self.window,
                0,
                0,
                &[x11rb::protocol::xproto::Rectangle {
                    x: 0,
                    y: 0,
                    width: self.size.0 as u16,
                    height: self.size.1 as u16,
                }],
            )?;
        }
        Ok(())
    }

    fn handle_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Expose(event) if event.count == 0 => {
                self.last_key.clear();
            }
            Event::ButtonPress(event) if event.detail == 1 => {
                let geometry = self.connection.get_geometry(self.window)?.reply()?;
                self.pending_manual = None;
                self.dragging = Some((
                    event.root_x,
                    event.root_y,
                    geometry.x as i32,
                    geometry.y as i32,
                ));
            }
            Event::MotionNotify(event) => {
                if let Some((start_x, start_y, origin_x, origin_y)) = self.dragging {
                    let screen = &self.connection.setup().roots[self.screen_num];
                    let target = (
                        origin_x + (event.root_x - start_x) as i32,
                        origin_y + (event.root_y - start_y) as i32,
                    );
                    let (x, y) = clamp_position(
                        target,
                        self.size,
                        (screen.width_in_pixels, screen.height_in_pixels),
                    );
                    if (x, y) != self.position {
                        self.position = (x, y);
                        self.connection.configure_window(
                            self.window,
                            &x11rb::protocol::xproto::ConfigureWindowAux::new().x(x).y(y),
                        )?;
                    }
                }
            }
            Event::ButtonRelease(_) if self.dragging.take().is_some() => {
                let position = [self.position.0, self.position.1];
                // 保持本地位置直到 daemon 把 Manual 配置广播回来。
                self.pending_manual = Some(position);
                let _ = self.moves.send(position);
            }
            _ => {}
        }
        Ok(())
    }
}

fn intern<C: x11rb::connection::Connection>(connection: &C, name: &[u8]) -> Result<u32> {
    Ok(connection.intern_atom(false, name)?.reply()?.atom)
}

fn x11_loop(
    config: Config,
    frames: Receiver<FrameUpdate>,
    store: Option<ConfigStore>,
    moves: tokio::sync::mpsc::UnboundedSender<[i32; 2]>,
) {
    let mut context = match WindowContext::new(&config, moves.clone()) {
        Ok(context) => context,
        Err(error) => {
            eprintln!("overlay: cannot create the X11 window: {error:#}");
            return;
        }
    };
    let _ = context.apply_click_through(config.overlay.click_through);
    let mut last_frame: Option<RenderedFrame> = None;
    loop {
        if let Err(error) = pump_events(&mut context) {
            eprintln!("overlay: event loop error: {error:#}");
            break;
        }
        let mut drew = false;
        loop {
            match frames.try_recv() {
                Ok(update) => {
                    match context.draw(&update) {
                        Ok(_) => {
                            last_frame = Some(update.frame);
                            drew = true;
                        }
                        Err(error) if error.to_string().contains("window mode changed") => {
                            // show_in_taskbar / always_on_top 切换：用新配置重建窗口，
                            // 重建完成后再重画一次最后一帧。
                            drop(context);
                            let next_config = store
                                .as_ref()
                                .and_then(|store| store.load().ok())
                                .unwrap_or(config.clone());
                            context = match WindowContext::new(&next_config, moves.clone()) {
                                Ok(ctx) => ctx,
                                Err(error) => {
                                    eprintln!("overlay: cannot recreate window: {error:#}");
                                    return;
                                }
                            };
                            let _ = context.apply_click_through(next_config.overlay.click_through);
                            if let Some(frame) = &last_frame {
                                let redo = FrameUpdate {
                                    frame: frame.clone(),
                                    click_through: next_config.overlay.click_through,
                                    visible: next_config.overlay.visible,
                                    position_preset: next_config.overlay.position_preset,
                                    manual_position: next_config.overlay.position,
                                    always_on_top: next_config.overlay.always_on_top,
                                };
                                let _ = context.draw(&redo);
                            }
                            drew = true;
                        }
                        Err(error) => {
                            eprintln!("overlay: cannot submit a frame: {error:#}");
                            return;
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                // D-Bus 循环已退出（daemon 退出或断连）：关闭悬浮窗，否则进程和窗口会一直残留。
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        if !drew {
            // 拖动时缩短轮询间隔，避免 10ms 的批量延迟叠加到指针移动上。
            let interval = if context.dragging.is_some() {
                Duration::from_millis(1)
            } else {
                Duration::from_millis(10)
            };
            thread::sleep(interval);
        }
    }
}

fn pump_events(context: &mut WindowContext) -> Result<()> {
    let mut handled = false;
    while let Some(event) = context.connection.poll_for_event()? {
        context.handle_event(event)?;
        handled = true;
    }
    // `configure_window` 这类无回复请求默认留在 x11rb 的 16KB 发送缓冲里，
    // 不主动 flush 的话拖动要等下一帧绘制（约 1s）或缓冲写满才生效。
    if handled {
        context.connection.flush()?;
    }
    Ok(())
}

fn empty_snapshot() -> Snapshot {
    Snapshot::new(
        0,
        trafficview_core::NetSample {
            iface: String::new(),
            rx_bps: 0,
            tx_bps: 0,
            rx_total: 0,
            tx_total: 0,
        },
        trafficview_core::CpuSample {
            total_pct: 0.0,
            per_core: Vec::new(),
            freq_mhz: None,
            load: None,
        },
        trafficview_core::MemSample {
            used_pct: 0.0,
            total: 0,
            available: 0,
            swap_pct: 0.0,
        },
    )
}

/// 估算初始窗口尺寸时假定可选显示项已有读数，避免首帧高度跳变。
fn sizing_snapshot() -> Snapshot {
    let mut snapshot = empty_snapshot();
    snapshot.cpu_temp = Some(0.0);
    snapshot.gpu = Some(trafficview_core::GpuSample {
        name: "GPU".into(),
        util_pct: 0.0,
        mem_used: 0,
        mem_total: 0,
        freq_mhz: None,
        temp_celsius: Some(0.0),
        stale: false,
        observed_at_ms: None,
    });
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_window_back_into_the_screen() {
        assert_eq!(clamp_position((-50, -20), (200, 100), (1920, 1080)), (0, 0));
        assert_eq!(
            clamp_position((1900, 1000), (200, 100), (1920, 1080)),
            (1720, 980)
        );
    }

    #[test]
    fn dpi_scales_layout_dimensions() {
        let items = Config::default().overlay.items;
        let base = trafficview_render::layout(&items, &empty_snapshot(), 96);
        let hidpi = trafficview_render::layout(&items, &empty_snapshot(), 144);
        assert_eq!(hidpi.width, base.width * 3 / 2);
        assert_eq!(hidpi.height, base.height * 3 / 2);
    }

    #[test]
    fn initial_sizing_accounts_for_optional_items() {
        let items = Config::default().overlay.items;
        let empty = trafficview_render::layout(&items, &empty_snapshot(), 96);
        let sizing = trafficview_render::layout(&items, &sizing_snapshot(), 96);
        assert_eq!(sizing.items.len(), 5);
        assert!(sizing.height > empty.height);
    }
}
