//! Terminal emulator model — the text surfaces behind the GUI Terminal windows.
//!
//! PICKLE OS has an in-kernel shell that talks to the world through the
//! `print!`/`println!` macros. Before the graphical desktop existed those went
//! straight to the VGA text buffer (and later to the framebuffer text console).
//! Once the compositor takes over the screen the raw text console is suspended,
//! so we need somewhere for shell output to live that the window manager can
//! draw inside a window.
//!
//! This module is that somewhere. It hosts a small fixed pool of independent
//! terminals (see [`MAX_TERMS`]) so the desktop can show several Terminal
//! windows at once, each driven by its own shell task. Every terminal is a
//! fixed-size character grid with a cursor and the usual control-character
//! handling (`\n`, `\r`, `\t`, backspace), line scrolling, a monotonic
//! per-terminal change counter ([`seq`]) the compositor uses to detect dirtiness
//! cheaply, and an `owner` task id.
//!
//! On top of the plain grid the terminal understands a useful subset of **ANSI
//! escape sequences** — SGR colour selection (`ESC[31m`, `ESC[92m`, `ESC[0m`),
//! cursor positioning (`ESC[H`, `ESC[row;colH`), and erase (`ESC[2J`, `ESC[K`) —
//! so programs (and the `colors` builtin) can paint coloured, addressable text.
//! Each cell therefore carries a foreground colour alongside its glyph.
//!
//! Each terminal also keeps a **scrollback buffer**: rows that scroll off the
//! top are retained (up to [`SCROLLBACK_MAX`]) and the user can page back
//! through them (PageUp/PageDown in the shell) via a per-terminal view offset.
//!
//! Output routing: console output is keyed to the *running task*. A shell binds
//! itself to a terminal with [`bind`]; thereafter anything it prints (via
//! [`write_for_current`], called from `vga_buffer::_print`) lands in that
//! terminal. Tasks with no terminal of their own (boot/init/driver logs) fall
//! back to the primary terminal (id 0) so their output stays visible.
//!
//! Input routing lives in the compositor + shell: only the shell that owns the
//! currently *focused* terminal ([`focus`]) drains the shared keyboard queue.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use spin::Mutex;
use alloc::vec::Vec;

/// Terminal grid dimensions, chosen to fit a Terminal window's client area
/// (see `gui.rs`). Each cell is one 8x8 glyph.
pub const COLS: usize = 64;
pub const ROWS: usize = 28;

/// Maximum number of simultaneous terminals (and thus Terminal windows).
pub const MAX_TERMS: usize = 4;

/// Sentinel "no terminal" id (e.g. when a non-terminal window has focus).
pub const NO_TERM: usize = usize::MAX;

/// The primary terminal: used as the fallback output sink for tasks that do
/// not own a terminal of their own (kernel/init/driver logs).
const PRIMARY: usize = 0;

/// Tab stop width in columns.
const TAB: usize = 4;

/// How many scrolled-off lines each terminal retains for scrollback.
pub const SCROLLBACK_MAX: usize = 200;

/// Foreground-colour sentinel meaning "use the window's default terminal
/// colour". Real ANSI colours below are all non-zero (ANSI black is mapped to a
/// near-black so it never collides with this sentinel).
pub const DEFAULT_FG: u32 = 0;

/// One terminal cell: a glyph plus its foreground colour (`0` = default).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: u32,
}

impl Cell {
    /// A blank cell in the default colour.
    pub const BLANK: Cell = Cell { ch: ' ', fg: DEFAULT_FG };
}

/// Standard ANSI colours (codes 30..37) tuned to read well on a dark console.
/// Index 0 (black) is a near-black so it never equals [`DEFAULT_FG`].
const ANSI: [u32; 8] = [
    0x1A1A1A, // black
    0xE05561, // red
    0x8CC265, // green
    0xE0C060, // yellow
    0x5C9CE6, // blue
    0xC162DE, // magenta
    0x4CC9C0, // cyan
    0xD7DAE0, // white
];

/// Bright ANSI colours (codes 90..97).
const ANSI_BRIGHT: [u32; 8] = [
    0x6B7280, // bright black (gray)
    0xFF7A85, // bright red
    0xB4F08C, // bright green
    0xFFE08A, // bright yellow
    0x8AB8FF, // bright blue
    0xE08CFF, // bright magenta
    0x80F0E8, // bright cyan
    0xFFFFFF, // bright white
];

/// Escape-sequence parser state.
const ESC_NONE: u8 = 0; // ordinary characters
const ESC_SEEN: u8 = 1; // just saw the ESC byte (0x1b)
const ESC_CSI: u8 = 2; // inside a CSI sequence: ESC [ ... <final>

struct Terminal {
    /// Character grid, row-major.
    cells: [[Cell; COLS]; ROWS],
    /// Cursor column / row.
    cx: usize,
    cy: usize,
    /// Whether this slot is allocated to a live window/shell.
    in_use: bool,
    /// Task id of the shell that owns this terminal (0 = none). Used both for
    /// output routing and to let a shell notice its window was closed.
    owner: u64,
    /// Current pen colour applied to newly-written glyphs (`0` = default).
    cur_fg: u32,
    /// ANSI escape parser state (`ESC_*`).
    esc: u8,
    /// Numeric parameters accumulated during a CSI sequence.
    params: [u16; 8],
    /// Index of the parameter currently being accumulated.
    nparam: usize,
    /// Whether any digit has been seen for the current sequence.
    have_digit: bool,
    /// Scrolled-off rows, oldest first (capped at [`SCROLLBACK_MAX`]).
    scroll: Vec<[Cell; COLS]>,
    /// How many lines the user has scrolled back from the live bottom (0 =
    /// showing the live grid).
    view: usize,
}

const EMPTY_TERM: Terminal = Terminal {
    cells: [[Cell::BLANK; COLS]; ROWS],
    cx: 0,
    cy: 0,
    in_use: false,
    owner: 0,
    cur_fg: DEFAULT_FG,
    esc: ESC_NONE,
    params: [0; 8],
    nparam: 0,
    have_digit: false,
    scroll: Vec::new(),
    view: 0,
};

static TERMS: Mutex<[Terminal; MAX_TERMS]> = Mutex::new([EMPTY_TERM; MAX_TERMS]);

/// Per-terminal change counters — bumped on every visible change so the
/// compositor can detect dirtiness without holding the terminal lock.
const SEQ_INIT: AtomicU64 = AtomicU64::new(0);
static SEQ: [AtomicU64; MAX_TERMS] = [SEQ_INIT; MAX_TERMS];

/// Whether the terminal sink is the active console destination. Set true by the
/// compositor once it owns the screen; until then console output keeps going to
/// the framebuffer text console / serial as before.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// The currently focused terminal (the one that should receive keyboard input),
/// or [`NO_TERM`] when a non-terminal window holds focus.
static FOCUS: AtomicUsize = AtomicUsize::new(PRIMARY);

/// Queue of terminal ids awaiting a freshly-spawned shell task to adopt them.
/// The compositor pushes an id when it opens a Terminal window; each new shell
/// task pops one on startup and binds itself to it.
static PENDING_SHELLS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

// --- Global activation flag --------------------------------------------------

/// Enable (or disable) routing of console output into the terminal model.
pub fn set_active(on: bool) {
    ACTIVE.store(on, Ordering::Release);
}

/// True if the terminal model is the current console sink.
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

// --- Slot lifecycle ----------------------------------------------------------

/// Allocate a fresh terminal slot, returning its id. The slot is cleared and
/// marked in-use. Returns `None` if all [`MAX_TERMS`] slots are taken.
pub fn alloc_terminal() -> Option<usize> {
    let mut terms = TERMS.lock();
    for (id, t) in terms.iter_mut().enumerate() {
        if !t.in_use {
            t.reset_full();
            t.in_use = true;
            SEQ[id].fetch_add(1, Ordering::Release);
            return Some(id);
        }
    }
    None
}

/// Release a terminal slot (its window was closed). The owning shell will
/// observe this via [`owns`] and park itself.
pub fn free_terminal(id: usize) {
    if id >= MAX_TERMS {
        return;
    }
    let mut terms = TERMS.lock();
    terms[id].reset_full();
    terms[id].in_use = false;
    terms[id].owner = 0;
    drop(terms);
    SEQ[id].fetch_add(1, Ordering::Release);
}

/// True if the slot is allocated (a window is showing it).
pub fn is_alive(id: usize) -> bool {
    id < MAX_TERMS && TERMS.lock()[id].in_use
}

/// Bind a terminal to an owning shell task.
pub fn bind(id: usize, task_id: u64) {
    if id >= MAX_TERMS {
        return;
    }
    TERMS.lock()[id].owner = task_id;
}

/// True if `task_id` still owns terminal `id` (i.e. the window is alive and the
/// task is its bound shell). A shell uses this to detect that its window closed.
pub fn owns(id: usize, task_id: u64) -> bool {
    if id >= MAX_TERMS {
        return false;
    }
    let t = TERMS.lock();
    t[id].in_use && t[id].owner == task_id
}

// --- Focus -------------------------------------------------------------------

/// Set the focused terminal (the one receiving keyboard input), or [`NO_TERM`].
pub fn set_focus(id: usize) {
    FOCUS.store(id, Ordering::Release);
}

/// The currently focused terminal id (or [`NO_TERM`]).
pub fn focus() -> usize {
    FOCUS.load(Ordering::Acquire)
}

/// True if terminal `id` currently has input focus.
pub fn is_focused(id: usize) -> bool {
    FOCUS.load(Ordering::Acquire) == id
}

// --- Pending-shell queue -----------------------------------------------------

/// Enqueue a terminal id for the next freshly-spawned shell task to adopt.
pub fn request_shell(id: usize) {
    PENDING_SHELLS.lock().push(id);
}

/// Pop the next terminal id awaiting a shell (FIFO), if any.
pub fn take_pending_shell() -> Option<usize> {
    let mut p = PENDING_SHELLS.lock();
    if p.is_empty() {
        None
    } else {
        Some(p.remove(0))
    }
}

// --- Change counter ----------------------------------------------------------

/// Monotonic change counter for terminal `id` — bumps on any grid/cursor change.
pub fn seq(id: usize) -> u64 {
    if id >= MAX_TERMS {
        return 0;
    }
    SEQ[id].load(Ordering::Acquire)
}

// --- The terminal model ------------------------------------------------------

impl Terminal {
    /// Scroll the whole grid up one line, retaining the displaced top row in the
    /// scrollback buffer and blanking the new bottom row.
    fn scroll_up(&mut self) {
        if self.scroll.len() >= SCROLLBACK_MAX {
            self.scroll.remove(0);
        }
        self.scroll.push(self.cells[0]);
        for r in 1..ROWS {
            self.cells[r - 1] = self.cells[r];
        }
        self.cells[ROWS - 1] = [Cell::BLANK; COLS];
        if self.cy > 0 {
            self.cy -= 1;
        }
    }

    fn newline(&mut self) {
        self.cx = 0;
        self.cy += 1;
        if self.cy >= ROWS {
            self.scroll_up();
        }
    }

    /// Handle one already-decoded ordinary character (no escape processing).
    fn put(&mut self, c: char) {
        match c {
            '\n' => self.newline(),
            '\r' => self.cx = 0,
            '\t' => {
                let next = ((self.cx / TAB) + 1) * TAB;
                self.cx = next.min(COLS - 1);
            }
            // Backspace: move the cursor left one cell. The caller (shell)
            // typically follows with a space + backspace to erase the glyph.
            '\u{8}' => {
                if self.cx > 0 {
                    self.cx -= 1;
                } else if self.cy > 0 {
                    self.cy -= 1;
                    self.cx = COLS - 1;
                }
            }
            c if (c as u32) >= 0x20 => {
                if self.cx >= COLS {
                    self.newline();
                }
                self.cells[self.cy][self.cx] = Cell { ch: c, fg: self.cur_fg };
                self.cx += 1;
            }
            _ => {} // ignore other control characters
        }
    }

    /// Feed one character through the ANSI escape state machine.
    fn feed(&mut self, c: char) {
        match self.esc {
            ESC_SEEN => {
                if c == '[' {
                    self.esc = ESC_CSI;
                    self.params = [0; 8];
                    self.nparam = 0;
                    self.have_digit = false;
                } else {
                    // Unsupported escape; abandon and emit nothing.
                    self.esc = ESC_NONE;
                }
            }
            ESC_CSI => {
                if c.is_ascii_digit() {
                    let d = (c as u16) - ('0' as u16);
                    let slot = &mut self.params[self.nparam];
                    *slot = slot.saturating_mul(10).saturating_add(d);
                    self.have_digit = true;
                } else if c == ';' {
                    if self.nparam + 1 < self.params.len() {
                        self.nparam += 1;
                    }
                } else {
                    let count = if self.have_digit || self.nparam > 0 {
                        self.nparam + 1
                    } else {
                        0
                    };
                    self.csi_dispatch(c, count);
                    self.esc = ESC_NONE;
                }
            }
            _ => {
                if c == '\u{1b}' {
                    self.esc = ESC_SEEN;
                } else {
                    self.put(c);
                }
            }
        }
    }

    /// Act on a completed CSI sequence with `count` numeric parameters.
    fn csi_dispatch(&mut self, final_byte: char, count: usize) {
        match final_byte {
            // SGR — select graphic rendition (colours).
            'm' => {
                if count == 0 {
                    self.cur_fg = DEFAULT_FG;
                } else {
                    for i in 0..count {
                        self.sgr(self.params[i]);
                    }
                }
            }
            // Cursor position: ESC[row;colH (1-based), defaults to home.
            'H' | 'f' => {
                let row = if count > 0 && self.params[0] > 0 { self.params[0] } else { 1 };
                let col = if count > 1 && self.params[1] > 0 { self.params[1] } else { 1 };
                self.cy = (row as usize - 1).min(ROWS - 1);
                self.cx = (col as usize - 1).min(COLS - 1);
            }
            // Erase in display: ESC[2J clears the screen and homes the cursor.
            'J' => {
                let mode = if count > 0 { self.params[0] } else { 0 };
                if mode == 2 || mode == 3 {
                    self.clear_grid();
                } else {
                    self.erase_to_eol();
                    for r in (self.cy + 1)..ROWS {
                        self.cells[r] = [Cell::BLANK; COLS];
                    }
                }
            }
            // Erase in line: ESC[K erases from the cursor to end of line.
            'K' => self.erase_to_eol(),
            _ => {}
        }
    }

    /// Apply a single SGR parameter.
    fn sgr(&mut self, code: u16) {
        match code {
            0 | 39 => self.cur_fg = DEFAULT_FG,
            30..=37 => self.cur_fg = ANSI[(code - 30) as usize],
            90..=97 => self.cur_fg = ANSI_BRIGHT[(code - 90) as usize],
            _ => {} // bold/underline/background etc. ignored
        }
    }

    fn erase_to_eol(&mut self) {
        for c in self.cx..COLS {
            self.cells[self.cy][c] = Cell::BLANK;
        }
    }

    /// Clear just the visible grid (keeps scrollback), homing the cursor.
    fn clear_grid(&mut self) {
        self.cells = [[Cell::BLANK; COLS]; ROWS];
        self.cx = 0;
        self.cy = 0;
        self.view = 0;
    }

    /// Full reset: clear grid, drop scrollback, reset pen and parser.
    fn reset_full(&mut self) {
        self.cells = [[Cell::BLANK; COLS]; ROWS];
        self.cx = 0;
        self.cy = 0;
        self.cur_fg = DEFAULT_FG;
        self.esc = ESC_NONE;
        self.params = [0; 8];
        self.nparam = 0;
        self.have_digit = false;
        self.scroll.clear();
        self.view = 0;
    }
}

/// Append a string to terminal `id`, honouring control characters and ANSI
/// escapes. Snaps the view back to the live bottom.
pub fn write(id: usize, s: &str) {
    if id >= MAX_TERMS {
        return;
    }
    let mut terms = TERMS.lock();
    terms[id].view = 0;
    for c in s.chars() {
        terms[id].feed(c);
    }
    drop(terms);
    SEQ[id].fetch_add(1, Ordering::Release);
}

/// Append a string to the terminal owned by the *currently running* task, or to
/// the primary terminal if the running task owns none. This is the entry point
/// `vga_buffer::_print` uses so each shell's output lands in its own window.
pub fn write_for_current(s: &str) {
    let tid = crate::task::current_id_fast();
    let mut terms = TERMS.lock();
    let target = terms
        .iter()
        .position(|t| t.in_use && t.owner == tid)
        .unwrap_or(PRIMARY);
    terms[target].view = 0;
    for c in s.chars() {
        terms[target].feed(c);
    }
    drop(terms);
    SEQ[target].fetch_add(1, Ordering::Release);
}

/// Clear terminal `id` and home its cursor (keeps scrollback).
pub fn clear(id: usize) {
    if id >= MAX_TERMS {
        return;
    }
    TERMS.lock()[id].clear_grid();
    SEQ[id].fetch_add(1, Ordering::Release);
}

/// Clear the terminal owned by the currently running task (the `clear` builtin).
pub fn clear_for_current() {
    let tid = crate::task::current_id_fast();
    let mut terms = TERMS.lock();
    let target = terms
        .iter()
        .position(|t| t.in_use && t.owner == tid)
        .unwrap_or(PRIMARY);
    terms[target].clear_grid();
    drop(terms);
    SEQ[target].fetch_add(1, Ordering::Release);
}

// --- Scrollback view control -------------------------------------------------

/// Scroll terminal `id`'s view by `delta` lines: positive scrolls *up* into
/// older history, negative scrolls back down toward the live bottom. Clamped to
/// the available scrollback.
pub fn scroll_view(id: usize, delta: i32) {
    if id >= MAX_TERMS {
        return;
    }
    let mut terms = TERMS.lock();
    let max = terms[id].scroll.len() as i32;
    let v = (terms[id].view as i32 + delta).clamp(0, max) as usize;
    let changed = v != terms[id].view;
    terms[id].view = v;
    drop(terms);
    if changed {
        SEQ[id].fetch_add(1, Ordering::Release);
    }
}

/// Snap terminal `id` back to the live bottom.
pub fn scroll_to_bottom(id: usize) {
    if id >= MAX_TERMS {
        return;
    }
    let mut terms = TERMS.lock();
    if terms[id].view != 0 {
        terms[id].view = 0;
        drop(terms);
        SEQ[id].fetch_add(1, Ordering::Release);
    }
}

/// Copy terminal `id`'s currently-visible grid into `out` and return the cursor
/// `(col, row)`. While the view is scrolled back into history the cursor is
/// reported as `(usize::MAX, usize::MAX)` so the caller hides it. Used by the
/// compositor to render a Terminal window without holding the lock across slow
/// framebuffer writes.
pub fn copy_grid(id: usize, out: &mut [[Cell; COLS]; ROWS]) -> (usize, usize) {
    if id >= MAX_TERMS {
        *out = [[Cell::BLANK; COLS]; ROWS];
        return (0, 0);
    }
    let t = TERMS.lock();
    if t[id].view == 0 {
        *out = t[id].cells;
        (t[id].cx, t[id].cy)
    } else {
        let s = t[id].scroll.len();
        let view = t[id].view.min(s);
        let start = s - view;
        for r in 0..ROWS {
            let vidx = start + r;
            out[r] = if vidx < s {
                t[id].scroll[vidx]
            } else {
                t[id].cells[vidx - s]
            };
        }
        (usize::MAX, usize::MAX)
    }
}
