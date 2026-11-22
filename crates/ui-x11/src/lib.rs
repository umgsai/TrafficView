//! X11 ARGB override-redirect 悬浮窗：只消费 D-Bus 快照，不读 /proc。

use std::{
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use trafficview_core::{
    config::{Config, ConfigStore},
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
    #[zbus(signal)]
    fn sample(
        &self,
        snapshot: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::Result<()>;
}

pub struct FrameUpdate {
    pub frame: RenderedFrame,
    pub click_through: bool,
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
    let store = ConfigStore::xdg().ok();
    let initial = config.clone();
    let x11_thread = thread::spawn(move || x11_loop(initial, frame_rx, store));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("cannot start the overlay runtime")?;
    let result = runtime.block_on(dbus_loop(config, dpi, frame_tx));
    if let Err(error) = &result {
        eprintln!("overlay: {error:#}");
    }
    let _ = x11_thread.join();
    result
}

async fn dbus_loop(config: Config, dpi: u32, frames: Sender<FrameUpdate>) -> Result<()> {
    let mut skin_renderer = load_skin_renderer(&config)?;
    let connection = zbus::Connection::session()
        .await
        .context("cannot connect to the session bus; is trafficviewd running?")?;
    let proxy = Daemon1Proxy::new(&connection)
        .await
        .context("cannot reach trafficviewd; start the daemon first")?;
    let mut stream = proxy
        .receive_sample()
        .await
        .context("cannot subscribe to the Sample signal")?;
    while let Some(signal) = stream.next().await {
        let args = signal.args()?;
        let snapshot = trafficview_daemon::dbus::snapshot_from_dbus(&args.snapshot)?;
        let frame = match &mut skin_renderer {
            Some(renderer) => renderer
                .render(&snapshot, dpi as f32 / trafficview_render::BASE_DPI as f32)
                .map_err(|error| anyhow::anyhow!(error))?,
            None => trafficview_render::render(&snapshot, &config, dpi),
        };
        if frames
            .send(FrameUpdate {
                frame,
                click_through: config.overlay.click_through,
            })
            .is_err()
        {
            break;
        }
    }
    Ok(())
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
    size: (u32, u32),
    last_key: String,
    dragging: Option<(i16, i16, i32, i32)>,
}

impl WindowContext {
    fn new(config: &Config) -> Result<Self> {
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
        let window = connection.generate_id()?;
        let position = clamp_position(
            (config.overlay.position[0], config.overlay.position[1]),
            (
                trafficview_render::layout(&config.overlay.items, &empty_snapshot(), 96).width,
                trafficview_render::layout(&config.overlay.items, &empty_snapshot(), 96).height,
            ),
            (screen.width_in_pixels, screen.height_in_pixels),
        );
        let width =
            trafficview_render::layout(&config.overlay.items, &empty_snapshot(), 96).width as u16;
        let height =
            trafficview_render::layout(&config.overlay.items, &empty_snapshot(), 96).height as u16;
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

        let dock = intern(&connection, b"_NET_WM_WINDOW_TYPE_DOCK")?;
        let window_type = intern(&connection, b"_NET_WM_WINDOW_TYPE")?;
        connection.change_property(
            PropMode::REPLACE,
            window,
            window_type,
            AtomEnum::ATOM,
            32,
            1,
            &dock.to_ne_bytes(),
        )?;
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
        connection.map_window(window)?;
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
            size: (width as u32, height as u32),
            last_key: String::new(),
            dragging: None,
        })
    }

    fn draw(&mut self, update: &FrameUpdate) -> Result<()> {
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
        if frame.cache_key == self.last_key {
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
        self.apply_click_through(update.click_through)?;
        self.connection.flush()?;
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

    fn handle_event(&mut self, event: Event, store: Option<&ConfigStore>) -> Result<()> {
        match event {
            Event::Expose(event) if event.count == 0 => {
                self.last_key.clear();
            }
            Event::ButtonPress(event) if event.detail == 1 => {
                let geometry = self.connection.get_geometry(self.window)?.reply()?;
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
                        if let Some(store) = store {
                            if let Ok(mut config) = store.load() {
                                config.overlay.position = [x, y];
                                let _ = store.save(&config);
                            }
                        }
                    }
                }
            }
            Event::ButtonRelease(_) => {
                self.dragging = None;
            }
            _ => {}
        }
        Ok(())
    }
}

fn intern<C: x11rb::connection::Connection>(connection: &C, name: &[u8]) -> Result<u32> {
    Ok(connection.intern_atom(false, name)?.reply()?.atom)
}

fn x11_loop(config: Config, frames: Receiver<FrameUpdate>, store: Option<ConfigStore>) {
    let mut context = match WindowContext::new(&config) {
        Ok(context) => context,
        Err(error) => {
            eprintln!("overlay: cannot create the X11 window: {error:#}");
            return;
        }
    };
    let _ = context.apply_click_through(config.overlay.click_through);
    loop {
        if let Err(error) = pump_events(&mut context, store.as_ref()) {
            eprintln!("overlay: event loop error: {error:#}");
            break;
        }
        let mut drew = false;
        while let Ok(update) = frames.try_recv() {
            if let Err(error) = context.draw(&update) {
                eprintln!("overlay: cannot submit a frame: {error:#}");
                return;
            }
            drew = true;
        }
        if !drew {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn pump_events(context: &mut WindowContext, store: Option<&ConfigStore>) -> Result<()> {
    while let Some(event) = context.connection.poll_for_event()? {
        context.handle_event(event, store)?;
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
}
