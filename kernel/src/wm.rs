//! Window-server core — the mechanism half of the PICKLE OS display server.
//!
//! This module is the trusted core of the windowing system (see
//! `docs/DISPLAY_SERVER_ARCHITECTURE.md`). It owns the canonical registry of
//! windows, each window's **shared backing pixel buffer**, and each window's
//! **input event queue**. It is deliberately *mechanism only*: it has no policy
//! about where windows are placed or which one is focused — that lives in the
//! compositor / `displayd` (`crate::gui`).
//!
//! There are two clients of this module:
//!   * applications, which reach it through the `SYS_WIN_*` syscalls (see
//!     `crate::syscall`); and
//!   * the compositor, which reads the window list, blits each window's backing
//!     store to the framebuffer, and pushes input events back into the queues.
//!
//! ## Shared-memory model (layer 1)
//! For now the backing buffer is a kernel-heap `Vec<u32>` of `0x00RRGGBB`
//! pixels. A client builds a frame in its own memory and presents it with
//! [`commit`], which copies it into the backing store. Later phases replace this
//! copy with genuinely shared pages mapped into the client (a `Memory`
//! capability), at which point `commit` becomes a damage report — but the
//! client-visible contract does not change. See the architecture doc.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

/// Opaque, unforgeable window handle.
pub type WindowId = u64;

/// Client/server protocol opcodes and server→client event kinds.
///
/// These are shared by both transports: the active `SYS_WIN_*` syscall path and
/// the reserved IPC-message path (for a future ring-3 `displayd`). Keeping the
/// numbering in one place keeps the two source-compatible.
pub mod op {
    // --- client -> server requests -----------------------------------------
    pub const CREATE_WINDOW: u64 = 1;
    pub const DESTROY_WINDOW: u64 = 2;
    pub const COMMIT: u64 = 3; // present the current buffer / report damage
    pub const SET_TITLE: u64 = 4;
    pub const POLL_EVENT: u64 = 7;
    pub const WINDOW_INFO: u64 = 8;

    // --- server -> client events (WmEvent.kind) ----------------------------
    pub const EV_NONE: u32 = 0;
    pub const EV_MOUSE_MOVE: u32 = 0x10;
    pub const EV_MOUSE_DOWN: u32 = 0x11;
    pub const EV_MOUSE_UP: u32 = 0x12;
    pub const EV_KEY: u32 = 0x13;
    pub const EV_CLOSE: u32 = 0x14;
    pub const EV_FOCUS: u32 = 0x15;
    pub const EV_BLUR: u32 = 0x16;
}

/// Maximum client-area dimensions. Windows are size-capped because the kernel
/// heap is only ~1 MiB and each backing buffer costs `w*h*4` bytes.
pub const MAX_W: usize = 320;
pub const MAX_H: usize = 240;

/// Maximum number of simultaneously registered windows.
pub const MAX_WINDOWS: usize = 16;

/// Per-window input event queue capacity (oldest dropped when full).
pub const EVENT_QUEUE_CAP: usize = 64;

/// One input event delivered from the compositor to a client. Coordinates are
/// **window-local** pixels. Mirrors the 16-byte wire layout documented in the
/// architecture doc.
#[derive(Clone, Copy)]
pub struct WmEvent {
    pub kind: u32,
    pub x: i32,
    pub y: i32,
    /// Mouse button index (0=L, 1=R, 2=M) or Unicode codepoint for key events.
    pub arg: u32,
}

impl WmEvent {
    pub fn new(kind: u32, x: i32, y: i32, arg: u32) -> Self {
        WmEvent { kind, x, y, arg }
    }

    /// Serialize into the 16-byte little-endian wire form.
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..4].copy_from_slice(&self.kind.to_le_bytes());
        b[4..8].copy_from_slice(&self.x.to_le_bytes());
        b[8..12].copy_from_slice(&self.y.to_le_bytes());
        b[12..16].copy_from_slice(&self.arg.to_le_bytes());
        b
    }
}

/// The server-side record for a single window.
struct ServerWindow {
    id: WindowId,
    /// Task id of the client that created (and thus owns) this window.
    owner: u64,
    /// Current on-screen position of the client area, maintained by the
    /// compositor (a title-bar drag updates this via [`set_position`]).
    x: i32,
    y: i32,
    /// Client-area dimensions in pixels.
    w: usize,
    h: usize,
    title: String,
    /// Shared backing store: `w*h` pixels of `0x00RRGGBB`.
    pixels: Vec<u32>,
    /// Bumped on every [`commit`] so the compositor can detect a new frame.
    buf_seq: u64,
    /// Pending input events for the client to drain via [`poll_event`].
    events: VecDeque<WmEvent>,
}

/// Global window-server state.
struct WmState {
    windows: BTreeMap<WindowId, ServerWindow>,
    /// Ids of windows created since the compositor last drained this queue.
    pending_new: VecDeque<WindowId>,
    /// Ids of windows destroyed since the compositor last drained this queue.
    pending_destroy: VecDeque<WindowId>,
}

static STATE: Mutex<Option<WmState>> = Mutex::new(None);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Global "something changed, repaint" counter. Bumped on create/commit/destroy
/// so the compositor can cheaply tell whether a full redraw is warranted without
/// holding the registry lock.
static DIRTY: AtomicU64 = AtomicU64::new(0);

fn bump_dirty() {
    DIRTY.fetch_add(1, Ordering::Release);
}

/// Initialize the window-server core (needs the heap).
pub fn init() {
    *STATE.lock() = Some(WmState {
        windows: BTreeMap::new(),
        pending_new: VecDeque::new(),
        pending_destroy: VecDeque::new(),
    });
    crate::serial_println!("[wm] window-server core online (max {} windows, {}x{} max)", MAX_WINDOWS, MAX_W, MAX_H);
}

fn with_state<R>(f: impl FnOnce(&mut WmState) -> R) -> Option<R> {
    let mut guard = STATE.lock();
    guard.as_mut().map(f)
}

/// Current global dirty counter (see [`DIRTY`]).
pub fn dirty() -> u64 {
    DIRTY.load(Ordering::Acquire)
}

// --- Client -> server operations -------------------------------------------

/// Create a window owned by `owner` with a `w`x`h` client area and `title`.
/// Sizes are clamped to [`MAX_W`]/[`MAX_H`]. Returns the new id, or `None` if
/// the registry is full or sizes are zero.
pub fn create_window(owner: u64, w: usize, h: usize, title: &str) -> Option<WindowId> {
    if w == 0 || h == 0 {
        return None;
    }
    let w = w.min(MAX_W);
    let h = h.min(MAX_H);
    with_state(|s| {
        if s.windows.len() >= MAX_WINDOWS {
            return None;
        }
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let win = ServerWindow {
            id,
            owner,
            x: 0,
            y: 0,
            w,
            h,
            title: title.to_string(),
            pixels: vec![0u32; w * h],
            buf_seq: 0,
            events: VecDeque::new(),
        };
        s.windows.insert(id, win);
        s.pending_new.push_back(id);
        Some(id)
    })
    .flatten()
    .map(|id| {
        bump_dirty();
        id
    })
}

/// True if `task_id` owns window `id`. Kernel callers (task id 0 in practice
/// never owns) and the compositor use the unchecked helpers below.
pub fn is_owner(id: WindowId, task_id: u64) -> bool {
    with_state(|s| s.windows.get(&id).map(|w| w.owner == task_id).unwrap_or(false))
        .unwrap_or(false)
}

/// Present a client frame: copy up to `w*h` pixels from `src` into the window's
/// backing store and mark the scene dirty. Extra `src` pixels are ignored; a
/// short `src` leaves the remainder of the buffer unchanged. Returns false if
/// the window does not exist.
pub fn commit(id: WindowId, src: &[u32]) -> bool {
    let ok = with_state(|s| {
        if let Some(win) = s.windows.get_mut(&id) {
            let n = src.len().min(win.pixels.len());
            win.pixels[..n].copy_from_slice(&src[..n]);
            win.buf_seq = win.buf_seq.wrapping_add(1);
            true
        } else {
            false
        }
    })
    .unwrap_or(false);
    if ok {
        bump_dirty();
    }
    ok
}

/// Destroy a window: free its backing store and queue the id for the compositor
/// to tear down its frame. Returns false if the window did not exist.
pub fn destroy_window(id: WindowId) -> bool {
    let ok = with_state(|s| {
        if s.windows.remove(&id).is_some() {
            s.pending_destroy.push_back(id);
            true
        } else {
            false
        }
    })
    .unwrap_or(false);
    if ok {
        bump_dirty();
    }
    ok
}

/// Destroy every window owned by `owner`. Called when a task exits (normally
/// or because it was terminated by a fault) so the compositor never keeps
/// compositing — or delivering input to — a window whose client task is gone.
/// Returns the number of windows torn down.
pub fn destroy_windows_owned_by(owner: u64) -> usize {
    let removed = with_state(|s| {
        let ids: alloc::vec::Vec<WindowId> = s
            .windows
            .iter()
            .filter(|(_, w)| w.owner == owner)
            .map(|(&id, _)| id)
            .collect();
        for id in &ids {
            s.windows.remove(id);
            s.pending_destroy.push_back(*id);
        }
        ids.len()
    })
    .unwrap_or(0);
    if removed > 0 {
        bump_dirty();
    }
    removed
}

/// Set a window's title (used by `SET_TITLE`).
pub fn set_title(id: WindowId, title: &str) -> bool {
    let ok = with_state(|s| {
        if let Some(win) = s.windows.get_mut(&id) {
            win.title = title.to_string();
            true
        } else {
            false
        }
    })
    .unwrap_or(false);
    if ok {
        bump_dirty();
    }
    ok
}

/// Pop the next input event for window `id`, or `None` if the queue is empty.
pub fn poll_event(id: WindowId) -> Option<WmEvent> {
    with_state(|s| s.windows.get_mut(&id).and_then(|w| w.events.pop_front())).flatten()
}

/// Read back current geometry as `(x, y, w, h)`.
pub fn window_info(id: WindowId) -> Option<(i32, i32, usize, usize)> {
    with_state(|s| s.windows.get(&id).map(|w| (w.x, w.y, w.w, w.h))).flatten()
}

// --- Compositor-facing helpers ---------------------------------------------

/// Pop the id of a window created since the last call (FIFO), if any. The
/// compositor drains this to learn about new windows it must frame.
pub fn take_pending_new() -> Option<WindowId> {
    with_state(|s| s.pending_new.pop_front()).flatten()
}

/// Pop the id of a window destroyed since the last call (FIFO), if any.
pub fn take_pending_destroy() -> Option<WindowId> {
    with_state(|s| s.pending_destroy.pop_front()).flatten()
}

/// The client-area size + title of a window, for the compositor to size its
/// frame. Returns `(w, h, title)`.
pub fn window_size_title(id: WindowId) -> Option<(usize, usize, String)> {
    with_state(|s| s.windows.get(&id).map(|w| (w.w, w.h, w.title.clone()))).flatten()
}

/// True if window `id` still exists.
pub fn exists(id: WindowId) -> bool {
    with_state(|s| s.windows.contains_key(&id)).unwrap_or(false)
}

/// Record a new on-screen position for a window (compositor moved it). Stored
/// so input events can be reported in window-local coordinates.
pub fn set_position(id: WindowId, x: i32, y: i32) {
    let _ = with_state(|s| {
        if let Some(win) = s.windows.get_mut(&id) {
            win.x = x;
            win.y = y;
        }
    });
}

/// Push an input event onto a window's queue (called by the compositor). Drops
/// the oldest event if the queue is at [`EVENT_QUEUE_CAP`].
pub fn push_event(id: WindowId, ev: WmEvent) {
    let _ = with_state(|s| {
        if let Some(win) = s.windows.get_mut(&id) {
            if win.events.len() >= EVENT_QUEUE_CAP {
                win.events.pop_front();
            }
            win.events.push_back(ev);
        }
    });
}

/// Run `f` with read access to window `id`'s backing pixels and dimensions
/// `(w, h, &pixels)`. Used by the compositor to blit the window without copying.
/// Returns `f`'s result, or `None` if the window does not exist.
pub fn with_pixels<R>(id: WindowId, f: impl FnOnce(usize, usize, &[u32]) -> R) -> Option<R> {
    with_state(|s| s.windows.get(&id).map(|w| f(w.w, w.h, &w.pixels))).flatten()
}

/// Current backing-buffer sequence number for a window (bumped on each commit).
pub fn buf_seq(id: WindowId) -> u64 {
    with_state(|s| s.windows.get(&id).map(|w| w.buf_seq).unwrap_or(0)).unwrap_or(0)
}

/// Number of registered windows (diagnostic).
pub fn window_count() -> usize {
    with_state(|s| s.windows.len()).unwrap_or(0)
}

// --- Headless self-test ------------------------------------------------------

/// Exercise the window-server core end to end without a display, logging to the
/// serial port (`[wm-selftest]`). Mirrors the other boot-time self-tests so the
/// foundation can be verified on a headless QEMU run.
pub fn wm_selftest() {
    use crate::serial_println;
    let owner = crate::task::current_id();

    // 1. Create.
    let id = match create_window(owner, 64, 32, "selftest") {
        Some(id) => id,
        None => {
            serial_println!("[wm-selftest] FAIL: create_window returned None");
            return;
        }
    };
    serial_println!("[wm-selftest] created window {} (count={})", id, window_count());
    // Drain our own new-window notification so it does not reach the compositor
    // (this window is torn down again below).
    while let Some(n) = take_pending_new() {
        let _ = n;
    }

    // 2. Ownership + geometry.
    let owner_ok = is_owner(id, owner);
    let geom_ok = window_info(id).map(|(_, _, w, h)| w == 64 && h == 32).unwrap_or(false);

    // 3. Commit a frame and verify the dirty counter advanced.
    let before = dirty();
    let frame = vec![0x00FF00FFu32; 64 * 32];
    let commit_ok = commit(id, &frame);
    let dirty_ok = dirty() > before;

    // 4. Event round-trip: push from the "compositor" side, drain from the
    //    "client" side.
    push_event(id, WmEvent::new(op::EV_KEY, 0, 0, 'A' as u32));
    push_event(id, WmEvent::new(op::EV_MOUSE_DOWN, 3, 4, 0));
    let e1 = poll_event(id);
    let e2 = poll_event(id);
    let e3 = poll_event(id);
    let events_ok = matches!(e1, Some(e) if e.kind == op::EV_KEY && e.arg == 'A' as u32)
        && matches!(e2, Some(e) if e.kind == op::EV_MOUSE_DOWN && e.x == 3 && e.y == 4)
        && e3.is_none();

    // 5. Destroy + destroy-notification queue.
    let destroy_ok = destroy_window(id);
    let notified = {
        let mut found = false;
        while let Some(d) = take_pending_destroy() {
            if d == id {
                found = true;
            }
        }
        found
    };
    let gone_ok = !exists(id);

    let pass = owner_ok && geom_ok && commit_ok && dirty_ok && events_ok && destroy_ok && notified && gone_ok;
    serial_println!(
        "[wm-selftest] owner={} geom={} commit={} dirty={} events={} destroy={} notified={} gone={} => {}",
        owner_ok, geom_ok, commit_ok, dirty_ok, events_ok, destroy_ok, notified, gone_ok,
        if pass { "PASS" } else { "FAIL" }
    );
}
