use anyhow::{Context, bail};
use std::sync::mpsc as std_mpsc;
use tracing::{debug, info};
use x11rb::connection::Connection;
use x11rb::protocol::shm;
use x11rb::protocol::xproto::{ImageFormat, Screen};
use x11rb::rust_connection::RustConnection;

const BYTES_PER_PIXEL: u32 = 4; // BGRA
/// Number of pre-allocated frame buffers. 3 allows one filling from SHM,
/// one being encoded by GStreamer, and one spare to absorb timing jitter.
const POOL_SIZE: usize = 3;

/// A frame buffer checked out from the pool. When dropped (e.g. after
/// GStreamer finishes encoding), the backing Vec is returned to the pool
/// for reuse, eliminating per-frame allocation overhead (~8MB at 1080p).
pub struct PooledFrame {
    data: Vec<u8>,
    return_tx: std_mpsc::Sender<Vec<u8>>,
}

impl PooledFrame {
    pub fn len(&self) -> usize {
        self.data.len()
    }
}

impl AsRef<[u8]> for PooledFrame {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

impl Drop for PooledFrame {
    fn drop(&mut self) {
        let data = std::mem::take(&mut self.data);
        // Return buffer to pool. If pool is gone (resize/shutdown), the Vec
        // is simply freed.
        let _ = self.return_tx.send(data);
    }
}

pub struct ScreenCapture {
    conn: RustConnection,
    root: u32,
    width: u32,
    height: u32,
    _depth: u8,
    shm_seg: shm::Seg,
    _shm_id: i32,
    shm_ptr: *mut u8,
    shm_size: usize,
    /// Pool of reusable frame buffers to avoid per-frame allocation
    pool_tx: std_mpsc::Sender<Vec<u8>>,
    pool_rx: std_mpsc::Receiver<Vec<u8>>,
}

// SAFETY: The SHM pointer is only accessed through &mut self methods,
// so there is no concurrent access.
unsafe impl Send for ScreenCapture {}

impl ScreenCapture {
    pub fn new(x_display: &str) -> anyhow::Result<Self> {
        let (conn, screen_num) =
            RustConnection::connect(Some(x_display)).context("Failed to connect to X display")?;

        // Verify SHM extension is available
        shm::query_version(&conn)
            .context("SHM extension not available")?
            .reply()
            .context("SHM query_version failed")?;

        let screen: &Screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let width = screen.width_in_pixels as u32;
        let height = screen.height_in_pixels as u32;
        let depth = screen.root_depth;

        info!(width, height, depth, x_display, "Connected to X display");

        let shm_size = (width * height * BYTES_PER_PIXEL) as usize;

        // Create POSIX shared memory segment
        let shm_id = unsafe { libc::shmget(libc::IPC_PRIVATE, shm_size, libc::IPC_CREAT | 0o600) };
        if shm_id < 0 {
            bail!("shmget failed: {}", std::io::Error::last_os_error());
        }

        let shm_ptr = unsafe { libc::shmat(shm_id, std::ptr::null(), 0) };
        if shm_ptr == usize::MAX as *mut libc::c_void {
            unsafe { libc::shmctl(shm_id, libc::IPC_RMID, std::ptr::null_mut()) };
            bail!("shmat failed: {}", std::io::Error::last_os_error());
        }

        // Mark segment for removal once all processes detach
        unsafe { libc::shmctl(shm_id, libc::IPC_RMID, std::ptr::null_mut()) };

        let shm_seg = conn
            .generate_id()
            .context("Failed to generate SHM seg id")?;
        shm::attach(&conn, shm_seg, shm_id as u32, false)
            .context("SHM attach request failed")?
            .check()
            .context("SHM attach failed")?;

        debug!(shm_seg, shm_size, "SHM segment attached");

        // Pre-allocate frame buffers to avoid per-frame mmap/munmap syscalls.
        // At 1080p (8MB/frame), eliminating 120 syscalls/sec of allocation churn.
        let (pool_tx, pool_rx) = std_mpsc::channel();
        for _ in 0..POOL_SIZE {
            let _ = pool_tx.send(vec![0u8; shm_size]);
        }
        info!(
            pool_size = POOL_SIZE,
            frame_bytes = shm_size,
            "Frame buffer pool initialized"
        );

        Ok(Self {
            conn,
            root,
            width,
            height,
            _depth: depth,
            shm_seg,
            _shm_id: shm_id,
            shm_ptr: shm_ptr as *mut u8,
            shm_size,
            pool_tx,
            pool_rx,
        })
    }

    /// Capture a frame into a pooled buffer. The SHM data is copied once
    /// into a pre-allocated buffer from the pool, then passed to the encoder
    /// via `gst::Buffer::from_slice`. When GStreamer finishes encoding, the
    /// buffer is automatically returned to the pool for reuse.
    pub fn capture_frame(&mut self) -> anyhow::Result<PooledFrame> {
        shm::get_image(
            &self.conn,
            self.root,
            0,
            0,
            self.width as u16,
            self.height as u16,
            !0, // all planes
            ImageFormat::Z_PIXMAP.into(),
            self.shm_seg,
            0, // offset into SHM
        )
        .context("SHM GetImage request failed")?
        .reply()
        .context("SHM GetImage reply failed")?;

        // Check out a buffer from the pool. Falls back to fresh allocation
        // if all pooled buffers are still in-flight in the GStreamer pipeline.
        let mut data = self
            .pool_rx
            .try_recv()
            .unwrap_or_else(|_| vec![0u8; self.shm_size]);

        // Ensure buffer is the right size (may differ after resize)
        data.resize(self.shm_size, 0);

        // SAFETY: The SHM segment is valid and large enough for the full frame.
        // We hold &mut self so no other code can access the buffer concurrently.
        let shm_slice = unsafe { std::slice::from_raw_parts(self.shm_ptr, self.shm_size) };
        data.copy_from_slice(shm_slice);

        // X11 depth-24 returns BGRx (4th byte is undefined padding, not alpha).
        // nvh264enc is told the data is BGRA, so set alpha to 0xFF to prevent
        // random padding values from causing color distortion during GPU
        // BGRA→NV12 conversion.
        for pixel in data.chunks_exact_mut(4) {
            pixel[3] = 0xFF;
        }

        Ok(PooledFrame {
            data,
            return_tx: self.pool_tx.clone(),
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        let _ = shm::detach(&self.conn, self.shm_seg);
        let _ = self.conn.flush();
        unsafe {
            libc::shmdt(self.shm_ptr as *const libc::c_void);
        }
        debug!("SHM segment detached and cleaned up");
    }
}
