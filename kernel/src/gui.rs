//! PICKLE OS graphical desktop: a tiny compositing window manager.
//!
//! This is the visible half of the framebuffer GUI. It runs as an ordinary
//! kernel task ([`compositor_task`]) and draws directly to the linear
//! framebuffer ([`crate::framebuffer`]) — there is no separate display server
//! process yet, but the design mirrors one: the compositor owns the screen and
//! a list of [`Window`]s, polls the PS/2 mouse driver ([`crate::driver::mouse`])
//! for pointer input, and re-renders the scene when something changes.
//!
//! Features:
//!   * a desktop background + a bottom taskbar with a live window list;
//!   * stacked, **draggable** windows (grab a title bar and move it);
//!   * click-to-raise focus and a working close button;
//!   * a software mouse cursor with per-pixel "save-under" so it glides over
//!     the desktop without a full-screen redraw every frame.
//!
//! Because the kernel heap is only 1 MiB we cannot afford a full off-screen
//! back-buffer (a 1280x720x3 frame is ~2.7 MiB). Instead the compositor redraws
//! the scene directly on change and uses a small save-under buffer for the
//! cursor, which keeps motion flicker-free while staying within budget.

use crate::driver::{console, mouse};
use crate::framebuffer::{self as fb, Color};
use crate::shell;
use crate::task;
use crate::terminal;
use crate::wm;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

// --- Theme ------------------------------------------------------------------

const C_DESKTOP: Color = Color(0x0A3D4A); // teal background (matches boot screen)
const C_TASKBAR: Color = Color(0x101A24); // dark slate
const C_TASKBAR_HI: Color = Color(0x1E2E3E);
const C_TITLE_ACTIVE: Color = Color(0x2A6CB0); // focused window title bar (blue)
const C_TITLE_INACTIVE: Color = Color(0x3A4654); // unfocused title bar (gray)
const C_TITLE_TEXT: Color = Color(0xFFFFFF);
const C_BODY: Color = Color(0xEDF1F5); // window client area
const C_BODY_TEXT: Color = Color(0x10202C);
const C_BORDER: Color = Color(0x05121C);
const C_CLOSE: Color = Color(0xD24B4B); // close button
const C_ACCENT: Color = Color(0x36C39B); // pickle green accent

const TITLE_H: i32 = 20; // title-bar height in pixels
const TASKBAR_H: i32 = 28; // bottom taskbar height
const CLOSE_SZ: i32 = 14; // close-button square size

// "+ Terminal" launcher button on the taskbar.
const NEWTERM_X: i32 = 96;
const NEWTERM_W: i32 = 104;

// Terminal text metrics (one 8x8 glyph per cell).
const TERM_CHAR_W: i32 = 8;
const TERM_LINE_H: i32 = 10;
const TERM_PAD_X: i32 = 8; // left/right padding inside the client area
const TERM_PAD_Y: i32 = 4; // padding below the title bar

// Terminal window colours (a classic dark console).
const C_TERM_BG: Color = Color(0x0B1A12);
const C_TERM_FG: Color = Color(0x9CF0B0);
const C_TERM_CURSOR: Color = Color(0x36C39B);

// --- A window ---------------------------------------------------------------

/// What a window paints in its client area.
#[derive(PartialEq, Eq, Clone, Copy)]
enum WindowKind {
    /// Static informational text (the `body` lines).
    Info,
    /// A live terminal rendered from [`crate::terminal`].
    Terminal,
    /// A client window backed by the window-server core ([`crate::wm`]): the
    /// compositor draws the managed frame (title bar, border, close box) and
    /// blits the client's shared pixel buffer into the client area.
    Client,
}

struct Window {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    title: String,
    body: Vec<String>,
    accent: Color,
    kind: WindowKind,
    /// For [`WindowKind::Terminal`] windows, the backing terminal slot id (see
    /// [`crate::terminal`]). Unused ([`terminal::NO_TERM`]) for other kinds.
    term_id: usize,
    /// For [`WindowKind::Client`] windows, the window-server id (see
    /// [`crate::wm`]). Unused (0) for other kinds.
    client_id: wm::WindowId,
}

impl Window {
    fn new(x: i32, y: i32, w: i32, h: i32, title: &str, body: &[&str], accent: Color) -> Self {
        Window {
            x,
            y,
            w,
            h,
            title: title.to_string(),
            body: body.iter().map(|s| s.to_string()).collect(),
            accent,
            kind: WindowKind::Info,
            term_id: terminal::NO_TERM,
            client_id: 0,
        }
    }

    /// Build a terminal window sized to exactly hold the [`terminal`] grid,
    /// backed by terminal slot `term_id`.
    fn new_terminal(x: i32, y: i32, title: &str, accent: Color, term_id: usize) -> Self {
        let w = terminal::COLS as i32 * TERM_CHAR_W + TERM_PAD_X * 2;
        let h = TITLE_H + TERM_PAD_Y + terminal::ROWS as i32 * TERM_LINE_H + TERM_PAD_Y;
        Window {
            x,
            y,
            w,
            h,
            title: title.to_string(),
            body: Vec::new(),
            accent,
            kind: WindowKind::Terminal,
            term_id,
            client_id: 0,
        }
    }

    /// Build a client window framing a window-server window of client-area size
    /// `cw`x`ch`. The outer window is sized to the client area plus the title
    /// bar so the blit lands flush under the bar.
    fn new_client(x: i32, y: i32, cw: i32, ch: i32, title: &str, accent: Color, client_id: wm::WindowId) -> Self {
        Window {
            x,
            y,
            w: cw,
            h: TITLE_H + ch,
            title: title.to_string(),
            body: Vec::new(),
            accent,
            kind: WindowKind::Client,
            term_id: terminal::NO_TERM,
            client_id,
        }
    }

    /// Client-area origin + size on screen for a [`WindowKind::Client`] window.
    fn client_area(&self) -> (i32, i32, i32, i32) {
        (self.x, self.y + TITLE_H, self.w, self.h - TITLE_H)
    }

    /// Screen rectangle of the close button (top-right of the title bar).
    fn close_rect(&self) -> (i32, i32, i32, i32) {
        (self.x + self.w - CLOSE_SZ - 4, self.y + 3, CLOSE_SZ, CLOSE_SZ)
    }

    fn point_in_title(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + TITLE_H
    }

    fn point_in_window(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }

    fn point_in_close(&self, px: i32, py: i32) -> bool {
        let (cx, cy, cw, ch) = self.close_rect();
        px >= cx && px < cx + cw && py >= cy && py < cy + ch
    }

    /// Render the window. `active` selects the focused title-bar colour and
    /// `blink_on` controls whether a terminal cursor block is shown this frame.
    fn draw(&self, active: bool, blink_on: bool) {
        let (x, y, w, h) = (self.x as usize, self.y as usize, self.w as usize, self.h as usize);
        // Drop shadow for a little depth.
        fb::fill_rect(x + 4, y + 4, w, h, Color(0x04141E));
        // Client area + title bar.
        let body_bg = if self.kind == WindowKind::Terminal { C_TERM_BG } else { C_BODY };
        fb::fill_rect(x, y, w, h, body_bg);
        let title_col = if active { C_TITLE_ACTIVE } else { C_TITLE_INACTIVE };
        fb::fill_rect(x, y, w, TITLE_H as usize, title_col);
        // Accent stripe under the title bar.
        fb::fill_rect(x, y + TITLE_H as usize, w, 2, self.accent);
        // Title text (vertically centred in the 20px bar with the 8px font).
        fb::draw_string_fg(&self.title, x + 6, y + 6, C_TITLE_TEXT);
        // Close button.
        let (cx, cy, cw, ch) = self.close_rect();
        fb::fill_rect(cx as usize, cy as usize, cw as usize, ch as usize, C_CLOSE);
        fb::draw_string_fg("x", cx as usize + 3, cy as usize + 3, C_TITLE_TEXT);

        match self.kind {
            WindowKind::Info => {
                // Static body text.
                let mut ty = y + TITLE_H as usize + 8;
                for line in &self.body {
                    fb::draw_string_fg(line, x + 8, ty, C_BODY_TEXT);
                    ty += 11;
                }
            }
            WindowKind::Terminal => self.draw_terminal(active, blink_on),
            WindowKind::Client => self.draw_client(),
        }
        // Outer border.
        fb::draw_rect_outline(x, y, w, h, C_BORDER);
    }

    /// Blit a client window's shared pixel buffer into its client area. Pixels
    /// are `0x00RRGGBB`. If the window-server window has vanished, the area is
    /// left as the body background.
    fn draw_client(&self) {
        let (ox, oy, cw, ch) = self.client_area();
        wm::with_pixels(self.client_id, |bw, bh, px| {
            let draw_w = (cw as usize).min(bw);
            let draw_h = (ch as usize).min(bh);
            for row in 0..draw_h {
                let py = oy + row as i32;
                if py < 0 {
                    continue;
                }
                for col in 0..draw_w {
                    let pxl = px[row * bw + col];
                    fb::put_pixel((ox + col as i32) as usize, py as usize, Color(pxl));
                }
            }
        });
    }

    /// Render the live terminal grid into this window's client area.
    fn draw_terminal(&self, active: bool, blink_on: bool) {
        // Pull a consistent snapshot of this window's terminal grid + cursor.
        let mut grid = [[terminal::Cell::BLANK; terminal::COLS]; terminal::ROWS];
        let (cur_c, cur_r) = terminal::copy_grid(self.term_id, &mut grid);

        let ox = self.x + TERM_PAD_X;
        let oy = self.y + TITLE_H + TERM_PAD_Y;
        for (r, row) in grid.iter().enumerate() {
            let py = oy + r as i32 * TERM_LINE_H;
            for (c, &cell) in row.iter().enumerate() {
                let ch = cell.ch;
                if ch != ' ' && ch != '\0' {
                    let px = ox + c as i32 * TERM_CHAR_W;
                    let fg = if cell.fg == terminal::DEFAULT_FG {
                        C_TERM_FG
                    } else {
                        Color(cell.fg)
                    };
                    fb::draw_glyph_fg(ch, px as usize, py as usize, fg);
                }
            }
        }

        // Block cursor (only when focused, and only on the "on" half of blink).
        if active && blink_on && cur_r < terminal::ROWS && cur_c < terminal::COLS {
            let px = (ox + cur_c as i32 * TERM_CHAR_W) as usize;
            let py = (oy + cur_r as i32 * TERM_LINE_H) as usize;
            fb::fill_rect(px, py, TERM_CHAR_W as usize, TERM_CHAR_W as usize, C_TERM_CURSOR);
            // Draw the underlying glyph on top of the cursor for visibility.
            let ch = grid[cur_r][cur_c].ch;
            if ch != ' ' && ch != '\0' {
                fb::draw_glyph_fg(ch, px, py, C_TERM_BG);
            }
        }
    }
}

// --- Mouse cursor sprite -----------------------------------------------------

const CURSOR_W: usize = 12;
const CURSOR_H: usize = 19;

/// Classic arrow: `#` = black outline, `.` = white fill, space = transparent.
const CURSOR: [&str; CURSOR_H] = [
    "#           ",
    "##          ",
    "#.#         ",
    "#..#        ",
    "#...#       ",
    "#....#      ",
    "#.....#     ",
    "#......#    ",
    "#.......#   ",
    "#........#  ",
    "#.....####  ",
    "#..#..#     ",
    "#.# #..#    ",
    "##  #..#    ",
    "#    #..#   ",
    "     #..#   ",
    "      #..#  ",
    "      #..#  ",
    "       ##   ",
];

/// Per-frame save-under buffer for the cursor (pixels we overwrote last frame).
struct CursorSaver {
    valid: bool,
    x: i32,
    y: i32,
    pixels: [Color; CURSOR_W * CURSOR_H],
}

impl CursorSaver {
    const fn new() -> Self {
        CursorSaver {
            valid: false,
            x: 0,
            y: 0,
            pixels: [Color::BLACK; CURSOR_W * CURSOR_H],
        }
    }

    /// Save the screen pixels about to be covered by the cursor at (x, y).
    fn save(&mut self, x: i32, y: i32) {
        for row in 0..CURSOR_H {
            for col in 0..CURSOR_W {
                let sx = x + col as i32;
                let sy = y + row as i32;
                self.pixels[row * CURSOR_W + col] = if sx >= 0 && sy >= 0 {
                    fb::read_pixel(sx as usize, sy as usize)
                } else {
                    Color::BLACK
                };
            }
        }
        self.x = x;
        self.y = y;
        self.valid = true;
    }

    /// Restore the previously-saved pixels (erasing the old cursor).
    fn restore(&self) {
        if !self.valid {
            return;
        }
        for row in 0..CURSOR_H {
            for col in 0..CURSOR_W {
                let sx = self.x + col as i32;
                let sy = self.y + row as i32;
                if sx >= 0 && sy >= 0 {
                    fb::put_pixel(sx as usize, sy as usize, self.pixels[row * CURSOR_W + col]);
                }
            }
        }
    }
}

/// Draw the arrow sprite at (x, y).
fn draw_cursor(x: i32, y: i32) {
    for (row, line) in CURSOR.iter().enumerate() {
        for (col, ch) in line.chars().enumerate() {
            let color = match ch {
                '#' => Color::BLACK,
                '.' => Color::WHITE,
                _ => continue,
            };
            let sx = x + col as i32;
            let sy = y + row as i32;
            if sx >= 0 && sy >= 0 {
                fb::put_pixel(sx as usize, sy as usize, color);
            }
        }
    }
}

// --- Compositor --------------------------------------------------------------

/// Number of Terminal windows currently open.
fn terminal_window_count(windows: &[Window]) -> usize {
    windows.iter().filter(|w| w.kind == WindowKind::Terminal).count()
}

/// How many client (program-owned) windows are currently mapped.
fn client_window_count(windows: &[Window]) -> usize {
    windows.iter().filter(|w| w.kind == WindowKind::Client).count()
}

/// How many more terminals could be opened right now.
fn free_terminal_slots(windows: &[Window]) -> usize {
    terminal::MAX_TERMS.saturating_sub(terminal_window_count(windows))
}

/// Screen rectangle (x, y, w, h) of the "+ Terminal" taskbar button.
fn new_term_button_rect(screen_h: i32) -> (i32, i32, i32, i32) {
    let tb_y = screen_h - TASKBAR_H;
    (NEWTERM_X, tb_y + 4, NEWTERM_W, TASKBAR_H - 8)
}

/// True if (px, py) falls inside the "+ Terminal" taskbar button.
fn point_in_new_term(px: i32, py: i32, screen_h: i32) -> bool {
    let (bx, by, bw, bh) = new_term_button_rect(screen_h);
    px >= bx && px < bx + bw && py >= by && py < by + bh
}

/// Draw the desktop background + bottom taskbar (everything behind windows).
fn draw_desktop(screen_w: i32, screen_h: i32, windows: &[Window], focus: usize, blink_on: bool) {
    fb::clear(C_DESKTOP);

    // A little branding in the top-left corner of the desktop.
    fb::draw_string_scaled("PICKLE OS", 24, 24, C_ACCENT, 3);
    fb::draw_string_fg(
        "a memory-safe microkernel desktop, drawn pixel by pixel",
        24,
        56,
        Color(0xBfeAdF),
    );

    // Taskbar.
    let tb_y = (screen_h - TASKBAR_H) as usize;
    fb::fill_rect(0, tb_y, screen_w as usize, TASKBAR_H as usize, C_TASKBAR);
    fb::fill_rect(0, tb_y, screen_w as usize, 2, C_ACCENT);
    fb::draw_string_fg("PICKLE", 10, tb_y + 10, C_ACCENT);
    fb::draw_string_fg("OS", 10 + 7 * 8, tb_y + 10, C_TITLE_TEXT);

    // "+ Terminal" launcher button. Dimmed when no terminal slot is free.
    let nx = NEWTERM_X as usize;
    let nw = NEWTERM_W as usize;
    let can_spawn = free_terminal_slots(windows) > 0;
    let nt_col = if can_spawn { C_ACCENT } else { C_TITLE_INACTIVE };
    fb::fill_rect(nx, tb_y + 4, nw, (TASKBAR_H - 8) as usize, C_TASKBAR_HI);
    fb::draw_rect_outline(nx, tb_y + 4, nw, (TASKBAR_H - 8) as usize, nt_col);
    fb::draw_string_fg("+ Terminal", nx + 8, tb_y + 10, nt_col);

    // One button per window in the taskbar (focused one highlighted).
    let mut bx = (NEWTERM_X + NEWTERM_W + 10) as usize;
    for (i, win) in windows.iter().enumerate() {
        let bw = 150usize;
        let col = if i == focus { C_TASKBAR_HI } else { C_TASKBAR };
        fb::fill_rect(bx, tb_y + 4, bw, (TASKBAR_H - 8) as usize, col);
        fb::draw_rect_outline(bx, tb_y + 4, bw, (TASKBAR_H - 8) as usize, C_BORDER);
        let accent = if i == focus { win.accent } else { C_TITLE_INACTIVE };
        fb::fill_rect(bx + 4, tb_y + 8, 6, 6, accent);
        // Truncate the label to fit the button.
        let label: String = win.title.chars().take(16).collect();
        fb::draw_string_fg(&label, bx + 16, tb_y + 10, C_TITLE_TEXT);
        bx += bw + 6;
    }

    // Draw windows back-to-front; the focused window is always last (top).
    for (i, win) in windows.iter().enumerate() {
        win.draw(i == focus, blink_on);
    }
}

/// The window-manager / compositor task.
pub extern "C" fn compositor_task() -> ! {
    // Wait until the framebuffer is up (it is initialised very early in
    // `kernel_main`, so this is effectively immediate, but be defensive).
    while !fb::is_active() {
        task::yield_now();
    }

    // Take over the screen: stop the full-screen text console from drawing
    // (its output now goes to the serial port) so it cannot scribble over the
    // desktop we are about to composite.
    fb::suspend_console(true);

    let screen_w = fb::width() as i32;
    let screen_h = fb::height() as i32;
    mouse::set_bounds_and_center(screen_w, screen_h);

    // Seed the desktop with a few windows.
    let mut windows: Vec<Window> = Vec::new();
    windows.push(Window::new(
        40,
        120,
        330,
        180,
        "Welcome",
        &[
            "Welcome to the PICKLE OS desktop!",
            "",
            "This window manager runs as a kernel",
            "task and composites directly to the",
            "linear framebuffer.",
            "",
            "Drag a title bar to move a window.",
            "Click a window to focus it; [x] closes.",
            "Hit [+ Terminal] for another shell.",
        ],
        C_ACCENT,
    ));
    windows.push(Window::new(
        880,
        130,
        320,
        160,
        "System",
        &[
            "kernel   : PICKLE OS v0.1.0",
            "arch     : x86_64 (long mode)",
            "bootinfo : bootloader 0.11",
            "display  : linear framebuffer",
            "input    : PS/2 mouse (IRQ12)",
            "          + PS/2 keyboard (IRQ1)",
        ],
        Color(0x6CB02A),
    ));
    // The live, interactive terminal — wired straight to the in-kernel shell.
    // Allocate the primary terminal slot and advertise it for adoption by the
    // shell task spawned during boot (which is already waiting for an id).
    let primary = terminal::alloc_terminal().unwrap_or(0);
    windows.push(Window::new_terminal(300, 150, "Terminal - pickleos shell", Color(0xB0742A), primary));
    terminal::request_shell(primary);

    // Route shell/console output into the terminal model now that we own the
    // screen, so the in-kernel shell becomes visible inside the window above.
    terminal::set_active(true);

    let mut focus = windows.len() - 1; // terminal starts focused (topmost)
    terminal::set_focus(primary);
    let mut dragging: Option<(i32, i32)> = None; // (offset_x, offset_y) into focused win
    let mut prev_left = false;
    let mut last_mouse_seq = u64::MAX; // force first frame to render
    // Per-terminal change counters for dirty detection.
    let mut last_seq = [0u64; terminal::MAX_TERMS];
    for id in 0..terminal::MAX_TERMS {
        last_seq[id] = terminal::seq(id);
    }
    // Track the window-server dirty counter so a client commit / window
    // create / destroy triggers a recomposite.
    let mut last_wm_dirty = wm::dirty();
    // The window-server window (if any) that currently has keyboard focus, so a
    // focus change can deliver EV_FOCUS/EV_BLUR exactly once.
    let mut focused_client: Option<wm::WindowId> = None;
    // Tracks the left-button state as last delivered to a focused client, so we
    // can synthesize MOUSE_DOWN/MOUSE_UP edges in client-local coordinates.
    let mut prev_left_client = false;
    let mut saver = CursorSaver::new();

    // Cursor blink: PIT ticks ~18.2 Hz, so ~9 ticks ≈ 0.5 s per phase.
    let mut frame: u64 = 0;
    let mut blink_on = true;

    // Helper: is the focused window a terminal sitting on top of the stack?
    let term_on_top = |windows: &[Window], focus: usize| -> bool {
        !windows.is_empty()
            && focus == windows.len() - 1
            && windows[focus].kind == WindowKind::Terminal
    };

    // Initial full render.
    draw_desktop(screen_w, screen_h, &windows, focus, blink_on);
    {
        let m = mouse::state();
        saver.save(m.x, m.y);
        draw_cursor(m.x, m.y);
    }

    loop {
        let m = mouse::state();
        let mouse_changed = m.seq != last_mouse_seq;
        last_mouse_seq = m.seq;

        let (cx, cy) = (m.x, m.y);
        let mut scene_dirty = false;

        // ----------------------------------------------------------------
        // Reconcile the window-server (wm) state into the compositor scene.
        // Client programs create/destroy windows via syscalls, which only
        // mutate the wm bookkeeping. Here we mirror those changes into the
        // compositor's own Window list so they become visible and routable.
        // ----------------------------------------------------------------
        while let Some(id) = wm::take_pending_new() {
            if let Some((cw, ch, title)) = wm::window_size_title(id) {
                let cw = cw as i32;
                let ch = ch as i32;
                let n = client_window_count(&windows) as i32;
                let wx = (360 + 26 * n).clamp(0, screen_w - cw.max(1));
                let wy = (180 + 26 * n).clamp(0, screen_h - TITLE_H);
                let win = Window::new_client(wx, wy, cw, ch, &title, Color(0x2A7AB0), id);
                windows.push(win);
                focus = windows.len() - 1;
                // The window's drawable client area starts below the title bar.
                wm::set_position(id, wx, wy + TITLE_H);
                wm::push_event(id, wm::WmEvent::new(wm::op::EV_FOCUS, 0, 0, 0));
                scene_dirty = true;
            }
        }
        while let Some(id) = wm::take_pending_destroy() {
            if let Some(i) = windows
                .iter()
                .position(|w| w.kind == WindowKind::Client && w.client_id == id)
            {
                windows.remove(i);
                focus = windows.len().saturating_sub(1);
                if focused_client == Some(id) {
                    focused_client = None;
                }
                scene_dirty = true;
            }
        }
        // A client committed new pixels: refresh the scene so the blit shows.
        let cur_wm_dirty = wm::dirty();
        if cur_wm_dirty != last_wm_dirty {
            last_wm_dirty = cur_wm_dirty;
            scene_dirty = true;
        }

        if mouse_changed {
            let left_pressed = m.left && !prev_left;
            if let Some((ox, oy)) = dragging {
                if m.left {
                    // Continue dragging the focused (topmost) window.
                    let win = windows.last_mut().unwrap();
                    win.x = (cx - ox).clamp(0, screen_w - win.w);
                    win.y = (cy - oy).clamp(0, screen_h - TITLE_H);
                    // Keep the window server's notion of the client-area origin
                    // in sync so event coordinates stay correct while dragging.
                    if win.kind == WindowKind::Client {
                        wm::set_position(win.client_id, win.x, win.y + TITLE_H);
                    }
                    scene_dirty = true;
                } else {
                    dragging = None;
                }
            } else if left_pressed && point_in_new_term(cx, cy, screen_h) {
                // "+ Terminal" button: open another terminal window backed by a
                // fresh terminal slot, and spawn a shell task to drive it.
                if let Some(id) = terminal::alloc_terminal() {
                    let n = terminal_window_count(&windows) as i32;
                    let wx = (300 + 28 * n).clamp(0, screen_w - 200);
                    let wy = (150 + 28 * n).clamp(0, screen_h - 120);
                    let win = Window::new_terminal(wx, wy, "Terminal - pickleos shell", Color(0xB0742A), id);
                    windows.push(win);
                    focus = windows.len() - 1;
                    terminal::request_shell(id);
                    task::spawn_kernel_task("shell", shell::shell_task);
                    scene_dirty = true;
                }
            } else if left_pressed {
                // Hit-test from topmost to bottom.
                let mut hit: Option<usize> = None;
                for i in (0..windows.len()).rev() {
                    if windows[i].point_in_window(cx, cy) {
                        hit = Some(i);
                        break;
                    }
                }
                if let Some(i) = hit {
                    if windows[i].point_in_close(cx, cy) {
                        // Release the backing terminal slot (if any) so its shell
                        // task notices and parks, and the slot can be reused.
                        if windows[i].kind == WindowKind::Terminal {
                            terminal::free_terminal(windows[i].term_id);
                        } else if windows[i].kind == WindowKind::Client {
                            // Notify the client and tear down the wm window. The
                            // EV_CLOSE lets a well-behaved client exit cleanly.
                            let cid = windows[i].client_id;
                            wm::push_event(cid, wm::WmEvent::new(wm::op::EV_CLOSE, 0, 0, 0));
                            wm::destroy_window(cid);
                            if focused_client == Some(cid) {
                                focused_client = None;
                            }
                        }
                        windows.remove(i);
                        focus = windows.len().saturating_sub(1);
                        scene_dirty = true;
                    } else {
                        // Raise to top (focus) by moving to the end of the vec.
                        let win = windows.remove(i);
                        let start_title = win.point_in_title(cx, cy);
                        windows.push(win);
                        focus = windows.len() - 1;
                        if start_title {
                            let w = windows.last().unwrap();
                            dragging = Some((cx - w.x, cy - w.y));
                        }
                        scene_dirty = true;
                    }
                }
            }
            prev_left = m.left;
        }

        // Keep keyboard focus in sync with the topmost window: the focused
        // terminal (if any) receives input; flush stale keys on a focus change
        // so they do not spill into the newly-focused shell.
        let foc_term = if term_on_top(&windows, focus) {
            windows[focus].term_id
        } else {
            terminal::NO_TERM
        };
        if foc_term != terminal::focus() {
            terminal::set_focus(foc_term);
            console::flush();
        }

        // ----------------------------------------------------------------
        // Route input to the focused client window (if a Client is on top).
        // A terminal on top consumes keyboard input through the shell path
        // above, so client routing only applies when the top window is a
        // Client. Deliver focus/blur transitions, keyboard, and mouse.
        // ----------------------------------------------------------------
        let top_client: Option<wm::WindowId> = if !windows.is_empty()
            && focus == windows.len() - 1
            && windows[focus].kind == WindowKind::Client
        {
            Some(windows[focus].client_id)
        } else {
            None
        };
        if top_client != focused_client {
            if let Some(old) = focused_client {
                wm::push_event(old, wm::WmEvent::new(wm::op::EV_BLUR, 0, 0, 0));
            }
            if let Some(new_id) = top_client {
                wm::push_event(new_id, wm::WmEvent::new(wm::op::EV_FOCUS, 0, 0, 0));
                // Drop any stale keystrokes so they do not leak to the client.
                console::flush();
            }
            focused_client = top_client;
        }
        if let Some(cid) = top_client {
            // Keyboard: no shell is draining the console while a client is
            // focused, so the compositor forwards keys into its event queue.
            while let Some(ch) = console::read_char() {
                wm::push_event(cid, wm::WmEvent::new(wm::op::EV_KEY, 0, 0, ch as u32));
            }
            // Mouse: translate to client-local coordinates. The client area
            // origin is (win.x, win.y + TITLE_H).
            let win = &windows[focus];
            let lx = cx - win.x;
            let ly = cy - (win.y + TITLE_H);
            let in_client = lx >= 0 && ly >= 0 && lx < win.w && ly < (win.h - TITLE_H);
            if mouse_changed && in_client {
                wm::push_event(cid, wm::WmEvent::new(wm::op::EV_MOUSE_MOVE, lx, ly, 0));
                if m.left && !prev_left_client {
                    wm::push_event(cid, wm::WmEvent::new(wm::op::EV_MOUSE_DOWN, lx, ly, 0));
                } else if !m.left && prev_left_client {
                    wm::push_event(cid, wm::WmEvent::new(wm::op::EV_MOUSE_UP, lx, ly, 0));
                }
            }
            prev_left_client = m.left;
        }

        // Detect new shell output across every terminal. We track which (if any)
        // terminals changed so we can do a cheap single-window repaint when only
        // the focused, on-top terminal changed, and a full redraw otherwise.
        let mut changed_count = 0usize;
        let mut focused_changed = false;
        for id in 0..terminal::MAX_TERMS {
            let s = terminal::seq(id);
            if s != last_seq[id] {
                last_seq[id] = s;
                changed_count += 1;
                if foc_term == id {
                    focused_changed = true;
                }
            }
        }

        frame += 1;
        let new_blink = (frame / 9) % 2 == 0;
        let blink_changed = new_blink != blink_on;
        blink_on = new_blink;

        // Decide what (if anything) to repaint this frame.
        if scene_dirty {
            // Window geometry/stacking changed: full scene redraw.
            draw_desktop(screen_w, screen_h, &windows, focus, blink_on);
            saver.save(cx, cy);
            draw_cursor(cx, cy);
        } else if changed_count == 1 && focused_changed && term_on_top(&windows, focus) {
            // Only the focused, on-top terminal changed: redraw just that one
            // window in place — far cheaper than the whole desktop.
            saver.restore();
            windows[focus].draw(true, blink_on);
            saver.save(cx, cy);
            draw_cursor(cx, cy);
        } else if changed_count > 0 {
            // Some terminal changed (a background one, or several): full redraw
            // so every visible Terminal window reflects its latest contents.
            draw_desktop(screen_w, screen_h, &windows, focus, blink_on);
            saver.save(cx, cy);
            draw_cursor(cx, cy);
        } else if blink_changed && term_on_top(&windows, focus) {
            // Just the cursor blink phase flipped on the focused terminal.
            saver.restore();
            windows[focus].draw(true, blink_on);
            saver.save(cx, cy);
            draw_cursor(cx, cy);
        } else if mouse_changed {
            // Pure pointer move: erase old cursor, save new spot, redraw cursor.
            saver.restore();
            saver.save(cx, cy);
            draw_cursor(cx, cy);
        }

        // Pace the loop: ~1 PIT tick keeps the blink/poll responsive while
        // leaving plenty of CPU for the shell and other tasks.
        task::sleep_ticks(1);
    }
}

// --- Window-server self-test + demo client -----------------------------------

/// Kernel task that runs the window-server self-test once, in a valid task
/// context (so `task::current_id()` is meaningful for ownership checks), then
/// exits. Spawned at boot before the demo client so its create/destroy
/// notifications do not race the compositor's reconciliation.
pub extern "C" fn wm_selftest_task() -> ! {
    wm::wm_selftest();
    task::do_exit(0);
}

/// A tiny built-in demo "client" that exercises the full window-server path the
/// same way a user program would: it creates a window, paints an animated
/// gradient into a locally-owned pixel buffer, commits each frame, and drains
/// input events (logging them to the serial console). It is the kernel-side
/// twin of the `libpickleos::gui` client API and proves the create → commit →
/// event-delivery → destroy loop end to end.
pub extern "C" fn client_demo_task() -> ! {
    // Give the compositor a moment to come up and run the self-test first.
    task::sleep_ticks(10);

    const CW: usize = 160;
    const CH: usize = 120;
    let owner = task::current_id();
    let id = match wm::create_window(owner, CW, CH, "demo client") {
        Some(id) => id,
        None => {
            crate::serial_println!("[win-demo] create_window failed");
            task::do_exit(1);
        }
    };
    crate::serial_println!("[win-demo] created window {} ({}x{})", id, CW, CH);

    let mut buf = alloc::vec![0u32; CW * CH];
    let mut t: u32 = 0;
    loop {
        // If the window was closed (by the compositor's close button), stop.
        if !wm::exists(id) {
            crate::serial_println!("[win-demo] window {} gone; exiting", id);
            task::do_exit(0);
        }

        // Paint an animated diagonal gradient.
        for y in 0..CH {
            for x in 0..CW {
                let r = ((x + t as usize) & 0xFF) as u32;
                let g = ((y + (t as usize) / 2) & 0xFF) as u32;
                let b = ((x + y) & 0xFF) as u32;
                buf[y * CW + x] = (r << 16) | (g << 8) | b;
            }
        }
        wm::commit(id, &buf);

        // Drain and log any pending input/lifecycle events.
        while let Some(ev) = wm::poll_event(id) {
            match ev.kind {
                wm::op::EV_KEY => {
                    crate::serial_println!("[win-demo] key: {:#x}", ev.arg)
                }
                wm::op::EV_MOUSE_DOWN => {
                    crate::serial_println!("[win-demo] mouse down @ ({},{})", ev.x, ev.y)
                }
                wm::op::EV_MOUSE_UP => {
                    crate::serial_println!("[win-demo] mouse up @ ({},{})", ev.x, ev.y)
                }
                wm::op::EV_FOCUS => crate::serial_println!("[win-demo] focus"),
                wm::op::EV_BLUR => crate::serial_println!("[win-demo] blur"),
                wm::op::EV_CLOSE => {
                    crate::serial_println!("[win-demo] close requested; tearing down");
                    wm::destroy_window(id);
                    task::do_exit(0);
                }
                _ => {}
            }
        }

        t = t.wrapping_add(2);
        // ~6 fps is plenty for a gentle gradient and keeps CPU use modest.
        task::sleep_ticks(3);
    }
}
