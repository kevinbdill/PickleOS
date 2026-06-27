//! Client-side GUI library for PICKLE OS.
//!
//! This is the user-space companion to the kernel window server (`wm`) and the
//! compositor (`gui::compositor_task`). It wraps the window syscalls
//! (`SYS_WIN_CREATE`, `SYS_WIN_COMMIT`, `SYS_WIN_POLL`, `SYS_WIN_DESTROY`,
//! `SYS_WIN_INFO`) behind an ergonomic, allocation-light API so a program can:
//!
//!   1. open a window of a fixed pixel size,
//!   2. draw into an off-screen pixel buffer it owns,
//!   3. commit (present) that buffer to the screen,
//!   4. and react to mouse / keyboard / lifecycle events.
//!
//! Pixels are `0x00RRGGBB` (the top byte is ignored). The drawing model is
//! deliberately simple: the client owns a `Vec<u32>` framebuffer the size of
//! the window's client area, mutates it directly (or via the small helper
//! methods), and calls [`Window::commit`] to push it to the compositor.
//!
//! # Example
//! ```no_run
//! # use libpickleos::gui::{Window, Event};
//! let mut win = Window::create(160, 120, "hello").expect("create window");
//! win.clear(0x202830);
//! win.fill_rect(10, 10, 40, 30, 0xE0A030);
//! win.commit();
//! loop {
//!     while let Some(ev) = win.poll_event() {
//!         match ev {
//!             Event::Close => return,
//!             Event::Key(c) => { /* handle key */ }
//!             Event::MouseDown { x, y } => { /* handle click */ }
//!             _ => {}
//!         }
//!     }
//!     libpickleos::syscall::sys_sleep(2);
//! }
//! ```

use alloc::vec;
use alloc::vec::Vec;

use crate::syscall;

/// Event opcodes — must match the kernel `wm::op` event kinds.
mod ev {
    pub const EV_NONE: u32 = 0x00;
    pub const EV_MOUSE_MOVE: u32 = 0x10;
    pub const EV_MOUSE_DOWN: u32 = 0x11;
    pub const EV_MOUSE_UP: u32 = 0x12;
    pub const EV_KEY: u32 = 0x13;
    pub const EV_CLOSE: u32 = 0x14;
    pub const EV_FOCUS: u32 = 0x15;
    pub const EV_BLUR: u32 = 0x16;
}

/// A decoded input / lifecycle event delivered to a client window.
///
/// Coordinates are in client-local pixels (origin at the top-left of the
/// drawable area, *below* the title bar).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The pointer moved over the window's client area.
    MouseMove { x: i32, y: i32 },
    /// The left mouse button was pressed inside the client area.
    MouseDown { x: i32, y: i32 },
    /// The left mouse button was released inside the client area.
    MouseUp { x: i32, y: i32 },
    /// A character key was typed (decoded byte; e.g. `b'\n'` for Enter).
    Key(u8),
    /// The window gained keyboard focus.
    Focus,
    /// The window lost keyboard focus.
    Blur,
    /// The user asked to close the window (clicked the title-bar close box).
    Close,
}

impl Event {
    /// Decode a 16-byte wire event (as written by `SYS_WIN_POLL`) into an
    /// [`Event`]. The layout is: u32 kind, i32 x, i32 y, u32 arg (LE).
    fn from_bytes(b: &[u8; 16]) -> Option<Event> {
        let kind = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let x = i32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        let y = i32::from_le_bytes([b[8], b[9], b[10], b[11]]);
        let arg = u32::from_le_bytes([b[12], b[13], b[14], b[15]]);
        match kind {
            ev::EV_MOUSE_MOVE => Some(Event::MouseMove { x, y }),
            ev::EV_MOUSE_DOWN => Some(Event::MouseDown { x, y }),
            ev::EV_MOUSE_UP => Some(Event::MouseUp { x, y }),
            ev::EV_KEY => Some(Event::Key(arg as u8)),
            ev::EV_FOCUS => Some(Event::Focus),
            ev::EV_BLUR => Some(Event::Blur),
            ev::EV_CLOSE => Some(Event::Close),
            ev::EV_NONE => None,
            _ => None,
        }
    }
}

/// A client window plus its locally-owned pixel buffer.
///
/// The buffer is `width * height` `u32`s in row-major order; element
/// `[y * width + x]` is the pixel at `(x, y)`. Use the drawing helpers or index
/// [`Window::buffer_mut`] directly, then call [`Window::commit`] to present.
pub struct Window {
    id: u64,
    width: u32,
    height: u32,
    pixels: Vec<u32>,
}

impl Window {
    /// Create a window of `width` x `height` client pixels with the given
    /// `title`. Returns `None` if the window server refused (e.g. too large for
    /// the kernel's window memory budget, or the table is full).
    pub fn create(width: u32, height: u32, title: &str) -> Option<Window> {
        let id = syscall::sys_win_create(width, height, title);
        if id == u64::MAX {
            return None;
        }
        Some(Window {
            id,
            width,
            height,
            pixels: vec![0u32; (width as usize) * (height as usize)],
        })
    }

    /// The window-server id assigned to this window.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Client-area dimensions in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Mutable access to the raw pixel buffer (`0x00RRGGBB`, row-major).
    pub fn buffer_mut(&mut self) -> &mut [u32] {
        &mut self.pixels
    }

    /// Read-only access to the raw pixel buffer.
    pub fn buffer(&self) -> &[u32] {
        &self.pixels
    }

    /// Fill the entire buffer with a single color.
    pub fn clear(&mut self, color: u32) {
        for p in self.pixels.iter_mut() {
            *p = color;
        }
    }

    /// Set one pixel (bounds-checked; out-of-range coordinates are ignored).
    pub fn put_pixel(&mut self, x: i32, y: i32, color: u32) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let idx = (y as usize) * (self.width as usize) + (x as usize);
        self.pixels[idx] = color;
    }

    /// Fill an axis-aligned rectangle, clipped to the buffer bounds.
    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.width as i32);
        let y1 = (y + h).min(self.height as i32);
        let stride = self.width as usize;
        let mut py = y0;
        while py < y1 {
            let row = (py as usize) * stride;
            let mut px = x0;
            while px < x1 {
                self.pixels[row + px as usize] = color;
                px += 1;
            }
            py += 1;
        }
    }

    /// Present the current pixel buffer to the screen. Returns `true` on
    /// success.
    pub fn commit(&self) -> bool {
        syscall::sys_win_commit(self.id, &self.pixels) == 0
    }

    /// Poll for the next pending input/lifecycle event, if any. Returns `None`
    /// when the queue is empty. Call repeatedly to drain the queue each frame.
    pub fn poll_event(&self) -> Option<Event> {
        let mut buf = [0u8; 16];
        let r = syscall::sys_win_poll(self.id, &mut buf);
        if r == 1 {
            Event::from_bytes(&buf)
        } else {
            None
        }
    }

    /// Query the window's on-screen geometry. Returns `(x, y, w, h)` of the
    /// client area, or `None` on error.
    pub fn info(&self) -> Option<(i32, i32, u32, u32)> {
        let mut buf = [0u8; 16];
        if syscall::sys_win_info(self.id, &mut buf) != 0 {
            return None;
        }
        let w = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let h = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let x = i32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let y = i32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        Some((x, y, w, h))
    }

    /// Explicitly destroy the window. Also called automatically on drop.
    pub fn destroy(&mut self) {
        if self.id != u64::MAX {
            syscall::sys_win_destroy(self.id);
            self.id = u64::MAX;
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        self.destroy();
    }
}
