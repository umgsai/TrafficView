//! `wl_shm` 缓冲：memfd + mmap，格式固定为 `Argb8888`（小端即 BGRA 预乘）。

use std::{
    ffi::CString,
    fs::File,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    ptr::NonNull,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::{bail, Context, Result};
use wayland_client::{
    protocol::{wl_buffer, wl_shm},
    Dispatch, QueueHandle,
};

pub struct ShmBuffer {
    pub buffer: wl_buffer::WlBuffer,
    pub released: Arc<AtomicBool>,
    pub width: u32,
    pub height: u32,
    ptr: NonNull<u8>,
    len: usize,
    _file: File,
}

// SAFETY: 缓冲区只由 Wayland 事件循环线程写入；指针在 Drop 前保持有效。
unsafe impl Send for ShmBuffer {}

impl ShmBuffer {
    pub fn create<State>(
        shm: &wl_shm::WlShm,
        queue: &QueueHandle<State>,
        width: u32,
        height: u32,
    ) -> Result<Self>
    where
        State: Dispatch<wl_buffer::WlBuffer, Arc<AtomicBool>>
            + Dispatch<wayland_client::protocol::wl_shm_pool::WlShmPool, ()>
            + 'static,
    {
        let width = width.max(1);
        let height = height.max(1);
        let stride = (width * 4) as i32;
        let len = stride as usize * height as usize;
        let file = memfd(len)?;
        let pool = shm.create_pool::<(), State>(file.as_fd(), len as i32, queue, ());
        let released = Arc::new(AtomicBool::new(false));
        let buffer = pool.create_buffer::<Arc<AtomicBool>, State>(
            0,
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Argb8888,
            queue,
            released.clone(),
        );
        pool.destroy();
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if raw.is_null() || raw == libc::MAP_FAILED {
            bail!(
                "cannot mmap {len} bytes for the Wayland buffer: {}",
                std::io::Error::last_os_error()
            );
        }
        let ptr = NonNull::new(raw.cast::<u8>()).expect("mmap returned a non-null pointer");
        Ok(Self {
            buffer,
            released,
            width,
            height,
            ptr,
            len,
            _file: file,
        })
    }

    /// 将 BGRA 预乘帧拷贝到共享内存。
    pub fn write(&mut self, bgra: &[u8]) {
        let len = bgra.len().min(self.len);
        unsafe {
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), self.ptr.as_ptr(), len);
        }
    }

    pub fn is_released(&self) -> bool {
        self.released.load(Ordering::SeqCst)
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
    }
}

fn memfd(len: usize) -> Result<File> {
    let name = CString::new("trafficview-shm").expect("static name has no NUL");
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        bail!("memfd_create failed: {}", std::io::Error::last_os_error());
    }
    let file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    file.set_len(len as u64)
        .with_context(|| format!("cannot resize the Wayland buffer to {len} bytes"))?;
    Ok(file)
}

/// 空输入区域：悬浮窗完全穿透鼠标。
pub fn empty_input_region<State>(
    compositor: &wayland_client::protocol::wl_compositor::WlCompositor,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
    queue: &QueueHandle<State>,
) where
    State: Dispatch<wayland_client::protocol::wl_region::WlRegion, ()> + 'static,
{
    let region = compositor.create_region::<(), State>(queue, ());
    surface.set_input_region(Some(&region));
    region.destroy();
}
