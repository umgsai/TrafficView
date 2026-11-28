//! KDE/wlroots layer-shell 悬浮窗：overlay 层、负 exclusive zone、空输入区域。

use std::{
    collections::HashMap,
    os::fd::AsRawFd,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
        Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use trafficview_core::{config::Config, Snapshot};
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_output, wl_region, wl_registry, wl_shm,
        wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use crate::{
    shm::{empty_input_region, ShmBuffer},
    spawn_snapshot_stream, FrameSource, SnapshotUpdate,
};
use trafficview_core::ColorScheme;
use trafficview_render::AlertState;

struct OutputState {
    output: wl_output::WlOutput,
    width: i32,
    height: i32,
    scale: i32,
}

struct Window {
    surface: wl_surface::WlSurface,
    layer_surface: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    _fractional: Option<wp_fractional_scale_v1::WpFractionalScaleV1>,
    fractional_scale: Option<f64>,
    int_scale: i32,
    configured: bool,
    ready: bool,
    frame_cb: Option<wl_callback::WlCallback>,
    buffer: Option<ShmBuffer>,
}

struct State {
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1,
    fractional_manager: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    outputs: HashMap<u32, OutputState>,
    windows: HashMap<u32, Window>,
    source: FrameSource,
    snapshot_rx: Receiver<SnapshotUpdate>,
    latest: Option<Snapshot>,
    scheme: ColorScheme,
    alerts: AlertState,
    position: [i32; 2],
    visible: bool,
}

pub fn run(config: Config) -> Result<()> {
    let connection = Connection::connect_to_env()
        .context("cannot connect to Wayland; is WAYLAND_DISPLAY set?")?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&connection).context("cannot initialize Wayland globals")?;
    let queue_handle = queue.handle();

    let compositor = globals
        .bind::<wl_compositor::WlCompositor, State, ()>(&queue_handle, 4..=6, ())
        .context("wl_compositor is unavailable")?;
    let shm = globals
        .bind::<wl_shm::WlShm, State, ()>(&queue_handle, 1..=1, ())
        .context("wl_shm is unavailable")?;
    let layer_shell = globals
        .bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1, State, ()>(&queue_handle, 1..=4, ())
        .map_err(|error| anyhow!("layer-shell unsupported: {error}"))?;
    let fractional_manager = globals
        .bind::<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1, State, ()>(
            &queue_handle,
            1..=1,
            (),
        )
        .ok();

    let registry = globals.registry();
    let mut outputs = HashMap::new();
    for global in globals.contents().clone_list() {
        if global.interface == "wl_output" {
            let output = registry.bind::<wl_output::WlOutput, u32, State>(
                global.name,
                global.version.min(4),
                &queue_handle,
                global.name,
            );
            outputs.insert(
                global.name,
                OutputState {
                    output,
                    width: 0,
                    height: 0,
                    scale: 1,
                },
            );
        }
    }

    let source = FrameSource::from_config(&config)?;
    let mut state = State {
        compositor,
        shm,
        layer_shell,
        fractional_manager,
        outputs,
        windows: HashMap::new(),
        source,
        snapshot_rx: spawn_snapshot_stream(),
        latest: None,
        scheme: ColorScheme::Dark,
        alerts: AlertState::default(),
        position: config.overlay.position,
        visible: config.overlay.visible,
    };

    queue
        .roundtrip(&mut state)
        .context("cannot receive initial output information")?;
    if state.visible {
        state.create_windows(&queue_handle)?;
    }
    queue
        .roundtrip(&mut state)
        .context("cannot receive layer-shell configure")?;
    eprintln!(
        "layer-shell overlay ready ({} outputs)",
        state.outputs.len()
    );

    loop {
        while let Ok(update) = state.snapshot_rx.try_recv() {
            state.latest = Some(update.snapshot);
            state.scheme = update.scheme;
            state.alerts = update.alerts;
            state.visible = update.visible;
        }
        state.apply_visibility(&queue_handle)?;
        pump(&connection, &mut queue, &mut state)?;
        state.render_windows(&queue_handle)?;
        connection
            .flush()
            .context("cannot flush Wayland requests")?;
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn pump(connection: &Connection, queue: &mut EventQueue<State>, state: &mut State) -> Result<()> {
    if let Some(guard) = connection.prepare_read() {
        let mut poll_fd = libc::pollfd {
            fd: guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll_fd, 1, 0) };
        if ready > 0 {
            let _ = guard.read();
        }
    }
    queue
        .dispatch_pending(state)
        .context("Wayland dispatch failed")?;
    Ok(())
}

impl State {
    fn create_windows(&mut self, queue: &QueueHandle<State>) -> Result<()> {
        let ids: Vec<u32> = self.outputs.keys().copied().collect();
        for id in ids {
            if self.windows.contains_key(&id) {
                continue;
            }
            let output = &self.outputs[&id];
            if output.width <= 0 || output.height <= 0 {
                continue;
            }
            let int_scale = output.scale.max(1);
            let logical_screen = [output.width / int_scale, output.height / int_scale];
            let canvas = self.source.logical_size();
            let anchored = crate::snap_anchor(
                self.position,
                logical_screen,
                [canvas[0] as i32, canvas[1] as i32],
                8,
            );
            let top = if anchored.anchor.0 & crate::Anchor::TOP.0 != 0 {
                anchored.margin.1
            } else {
                0
            };
            let bottom = if anchored.anchor.0 & crate::Anchor::BOTTOM.0 != 0 {
                anchored.margin.1
            } else {
                0
            };
            let left = if anchored.anchor.0 & crate::Anchor::LEFT.0 != 0 {
                anchored.margin.0
            } else {
                0
            };
            let right = if anchored.anchor.0 & crate::Anchor::RIGHT.0 != 0 {
                anchored.margin.0
            } else {
                0
            };

            let surface = self.compositor.create_surface::<(), State>(queue, ());
            let layer_surface = self.layer_shell.get_layer_surface::<u32, State>(
                &surface,
                Some(&output.output),
                zwlr_layer_shell_v1::Layer::Overlay,
                "trafficview".to_string(),
                queue,
                id,
            );
            let anchor = zwlr_layer_surface_v1::Anchor::from_bits_truncate(anchored.anchor.0);
            layer_surface.set_anchor(anchor);
            layer_surface.set_margin(top, right, bottom, left);
            layer_surface.set_exclusive_zone(-1);
            layer_surface
                .set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
            empty_input_region(&self.compositor, &surface, queue);
            let fractional = self
                .fractional_manager
                .as_ref()
                .map(|manager| manager.get_fractional_scale::<u32, State>(&surface, queue, id));
            surface.commit();

            self.windows.insert(
                id,
                Window {
                    surface,
                    layer_surface,
                    _fractional: fractional,
                    fractional_scale: None,
                    int_scale,
                    configured: false,
                    ready: false,
                    frame_cb: None,
                    buffer: None,
                },
            );
        }
        Ok(())
    }

    fn effective_scale(&self, id: u32) -> f64 {
        self.windows
            .get(&id)
            .map(|window| {
                window
                    .fractional_scale
                    .unwrap_or(window.int_scale.max(1) as f64)
            })
            .unwrap_or(1.0)
    }

    /// 托盘/设置窗口切换 `overlay.visible`：销毁或重建所有输出上的窗口。
    fn apply_visibility(&mut self, queue: &QueueHandle<State>) -> Result<()> {
        if self.visible {
            self.create_windows(queue)
        } else {
            for (_, window) in self.windows.drain() {
                window.layer_surface.destroy();
                window.surface.destroy();
            }
            Ok(())
        }
    }

    fn render_windows(&mut self, queue: &QueueHandle<State>) -> Result<()> {
        if !self.visible {
            return Ok(());
        }
        let Some(snapshot) = self.latest.clone() else {
            return Ok(());
        };
        let ids: Vec<u32> = self.windows.keys().copied().collect();
        for id in ids {
            let scale = self.effective_scale(id);
            let can_render = self.windows.get(&id).is_some_and(|window| {
                window.configured
                    && window.ready
                    && window.frame_cb.is_none()
                    && window.buffer.as_ref().is_none_or(ShmBuffer::is_released)
            });
            if !can_render {
                continue;
            }
            let frame = self
                .source
                .render(&snapshot, scale, self.scheme, &self.alerts)?;
            let Some(window) = self.windows.get_mut(&id) else {
                continue;
            };
            let needs_buffer = window
                .buffer
                .as_ref()
                .is_none_or(|buffer| buffer.width != frame.width || buffer.height != frame.height);
            if needs_buffer {
                window.buffer = Some(ShmBuffer::create(
                    &self.shm,
                    queue,
                    frame.width,
                    frame.height,
                )?);
            }
            let buffer = window
                .buffer
                .as_mut()
                .ok_or_else(|| anyhow!("Wayland buffer disappeared"))?;
            buffer.write(&frame.argb);
            buffer.released.store(false, Ordering::SeqCst);
            window.surface.attach(Some(&buffer.buffer), 0, 0);
            window
                .surface
                .damage(0, 0, frame.width as i32, frame.height as i32);
            let buffer_scale = if window.fractional_scale.is_some() {
                1
            } else {
                window.int_scale.max(1)
            };
            window.surface.set_buffer_scale(buffer_scale);
            window.frame_cb = Some(window.surface.frame::<u32, State>(queue, id));
            window.ready = false;
            window.surface.commit();
            window.layer_surface.set_exclusive_zone(-1);
        }
        Ok(())
    }
}

fn mark_ready(state: &mut State, id: u32) {
    if let Some(window) = state.windows.get_mut(&id) {
        window.frame_cb = None;
        window.ready = true;
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_compositor::WlCompositor,
        _event: wl_compositor::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm::WlShm,
        _event: wl_shm::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm_pool::WlShmPool,
        _event: wl_shm_pool::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_region::WlRegion, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_region::WlRegion,
        _event: wl_region::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_surface::WlSurface,
        _event: wl_surface::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, Arc<AtomicBool>> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        released: &Arc<AtomicBool>,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            released.store(true, Ordering::SeqCst);
        }
    }
}

impl Dispatch<wl_callback::WlCallback, u32> for State {
    fn event(
        state: &mut Self,
        _proxy: &wl_callback::WlCallback,
        event: wl_callback::Event,
        id: &u32,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            mark_ready(state, *id);
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        id: &u32,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        if let Some(output) = state.outputs.get_mut(id) {
            match event {
                wl_output::Event::Mode { width, height, .. } => {
                    output.width = width;
                    output.height = height;
                }
                wl_output::Event::Scale { factor } => output.scale = factor.max(1),
                _ => {}
            }
        }
    }
}

impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
        _event: zwlr_layer_shell_v1::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, u32> for State {
    fn event(
        state: &mut Self,
        proxy: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        id: &u32,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                proxy.ack_configure(serial);
                if let Some(window) = state.windows.get_mut(id) {
                    window.configured = true;
                    window.ready = true;
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.windows.remove(id);
            }
            _ => {}
        }
    }
}

impl Dispatch<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        _event: wp_fractional_scale_manager_v1::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, u32> for State {
    fn event(
        state: &mut Self,
        _proxy: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        id: &u32,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            if let Some(window) = state.windows.get_mut(id) {
                window.fractional_scale = Some(f64::from(scale) / 120.0);
                window.ready = true;
            }
        }
    }
}
