//! GNOME 等无 layer-shell 桌面的普通 `xdg_toplevel` 降级窗口。
//!
//! 明确不承诺贴边、置顶或任意定位；只保证透明内容、可关闭与可重连。

use std::{
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
        wl_buffer, wl_callback, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use crate::{shm::ShmBuffer, spawn_snapshot_stream, FrameSource};

struct Window {
    surface: wl_surface::WlSurface,
    xdg_surface: xdg_surface::XdgSurface,
    _toplevel: xdg_toplevel::XdgToplevel,
    configured: bool,
    ready: bool,
    frame_cb: Option<wl_callback::WlCallback>,
    buffer: Option<ShmBuffer>,
}

struct State {
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    source: FrameSource,
    snapshot_rx: Receiver<Snapshot>,
    latest: Option<Snapshot>,
    window: Option<Window>,
    closed: bool,
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
    let wm_base = globals
        .bind::<xdg_wm_base::XdgWmBase, State, ()>(&queue_handle, 1..=6, ())
        .context("xdg_wm_base is unavailable")?;

    let source = FrameSource::from_config(&config)?;
    let mut state = State {
        compositor,
        shm,
        source,
        snapshot_rx: spawn_snapshot_stream(),
        latest: None,
        window: None,
        closed: false,
    };
    state.create_window(&queue_handle, &wm_base)?;
    queue
        .roundtrip(&mut state)
        .context("cannot receive xdg configure")?;
    eprintln!("xdg_toplevel overlay ready (GNOME-safe fallback)");

    while !state.closed {
        while let Ok(snapshot) = state.snapshot_rx.try_recv() {
            state.latest = Some(snapshot);
        }
        pump(&connection, &mut queue, &mut state)?;
        state.render_window(&queue_handle)?;
        connection
            .flush()
            .context("cannot flush Wayland requests")?;
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
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
    fn create_window(
        &mut self,
        queue: &QueueHandle<State>,
        wm_base: &xdg_wm_base::XdgWmBase,
    ) -> Result<()> {
        let surface = self.compositor.create_surface::<(), State>(queue, ());
        let xdg_surface = wm_base.get_xdg_surface::<(), State>(&surface, queue, ());
        let toplevel = xdg_surface.get_toplevel::<(), State>(queue, ());
        toplevel.set_app_id("trafficview".to_string());
        toplevel.set_title("TrafficView".to_string());
        surface.commit();
        self.window = Some(Window {
            surface,
            xdg_surface,
            _toplevel: toplevel,
            configured: false,
            ready: false,
            frame_cb: None,
            buffer: None,
        });
        Ok(())
    }

    fn render_window(&mut self, queue: &QueueHandle<State>) -> Result<()> {
        let Some(snapshot) = self.latest.clone() else {
            return Ok(());
        };
        let Some(window) = self.window.as_mut() else {
            return Ok(());
        };
        if !window.configured || !window.ready || window.frame_cb.is_some() {
            return Ok(());
        }
        if window
            .buffer
            .as_ref()
            .is_some_and(|buffer| !buffer.is_released())
        {
            return Ok(());
        }
        let frame = self.source.render(&snapshot, 1.0)?;
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
        window
            .xdg_surface
            .set_window_geometry(0, 0, frame.width as i32, frame.height as i32);
        window.frame_cb = Some(window.surface.frame::<(), State>(queue, ()));
        window.ready = false;
        window.surface.commit();
        Ok(())
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

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            if let Some(window) = state.window.as_mut() {
                window.frame_cb = None;
                window.ready = true;
            }
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _state: &mut Self,
        proxy: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            proxy.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            proxy.ack_configure(serial);
            if let Some(window) = state.window.as_mut() {
                window.configured = true;
                window.ready = true;
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Close => state.closed = true,
            xdg_toplevel::Event::Configure { .. } => {
                if let Some(window) = state.window.as_mut() {
                    window.configured = true;
                }
            }
            _ => {}
        }
    }
}
