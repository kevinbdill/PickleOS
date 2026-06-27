//! Linear pixel framebuffer graphics driver.
//!
//! With the move to `bootloader 0.11`, the BIOS bootloader sets up a VESA
//! linear framebuffer and hands us a pointer + geometry in the [`BootInfo`].
//! This module wraps that framebuffer and provides:
//!
//!   * Low-level pixel ops: [`put_pixel`], [`fill_rect`], [`clear`].
//!   * A built-in 8x8 bitmap-font **text console** so the existing
//!     `print!`/`println!` macros keep working in graphics mode (the legacy
//!     `0xb8000` text buffer is *not* available once we are in a graphics
//!     mode, and may not even be mapped — touching it would fault).
//!
//! Drawing is done directly to the hardware framebuffer (no back buffer yet);
//! the window manager can add double buffering on top later.

use bootloader_api::info::{FrameBufferInfo, PixelFormat};
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

/// RGB color as `0x00RRGGBB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color(pub u32);

#[allow(dead_code)]
impl Color {
    pub const BLACK: Color = Color(0x000000);
    pub const WHITE: Color = Color(0xFFFFFF);
    pub const RED: Color = Color(0xFF0000);
    pub const GREEN: Color = Color(0x00FF00);
    pub const BLUE: Color = Color(0x0000FF);
    pub const CYAN: Color = Color(0x00FFFF);
    pub const MAGENTA: Color = Color(0xFF00FF);
    pub const YELLOW: Color = Color(0xFFFF00);
    pub const GRAY: Color = Color(0x808080);
    pub const DARK_GRAY: Color = Color(0x202020);
    /// PICKLE OS desktop background (deep teal).
    pub const DESKTOP: Color = Color(0x0A3D4A);

    #[inline]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color(((r as u32) << 16) | ((g as u32) << 8) | (b as u32))
    }

    #[inline]
    pub const fn r(self) -> u8 {
        (self.0 >> 16) as u8
    }
    #[inline]
    pub const fn g(self) -> u8 {
        (self.0 >> 8) as u8
    }
    #[inline]
    pub const fn b(self) -> u8 {
        self.0 as u8
    }
}

/// The active framebuffer. Raw pointer + geometry, guarded by a spinlock.
struct FrameBuffer {
    ptr: *mut u8,
    info: FrameBufferInfo,
}

// SAFETY: the framebuffer is a single MMIO region; all access is serialized
// through the `FB` mutex.
unsafe impl Send for FrameBuffer {}

static FB: Mutex<Option<FrameBuffer>> = Mutex::new(None);

/// Text console state (cursor position + colors), separate lock from `FB` so a
/// `print!` only needs the console lock + the FB lock briefly per glyph.
struct Console {
    col: usize,
    row: usize,
    cols: usize,
    rows: usize,
    fg: Color,
    bg: Color,
    active: bool,
}

static CONSOLE: Mutex<Console> = Mutex::new(Console {
    col: 0,
    row: 0,
    cols: 0,
    rows: 0,
    fg: Color(0x00FF66), // phosphor green, matches the old VGA theme
    bg: Color::DESKTOP,
    active: false,
});

/// Glyph cell size for the built-in 8x8 font.
const GLYPH_W: usize = 8;
const GLYPH_H: usize = 8;

/// Initialise the graphics driver from the bootloader's framebuffer.
///
/// # Safety
/// `ptr` must point to the start of a valid linear framebuffer of at least
/// `info.byte_len` bytes that stays valid for the lifetime of the kernel.
pub fn init(ptr: *mut u8, info: FrameBufferInfo) {
    {
        let mut fb = FB.lock();
        *fb = Some(FrameBuffer { ptr, info });
    }
    {
        let mut con = CONSOLE.lock();
        con.cols = info.width / GLYPH_W;
        con.rows = info.height / GLYPH_H;
        con.col = 0;
        con.row = 0;
        con.active = true;
    }
    // Paint the desktop background so boot output is readable.
    clear(Color::DESKTOP);
}

/// True once [`init`] has run and a framebuffer is available.
pub fn is_active() -> bool {
    CONSOLE.lock().active
}

/// Screen width in pixels (0 if no framebuffer).
pub fn width() -> usize {
    FB.lock().as_ref().map(|f| f.info.width).unwrap_or(0)
}

/// Screen height in pixels (0 if no framebuffer).
pub fn height() -> usize {
    FB.lock().as_ref().map(|f| f.info.height).unwrap_or(0)
}

/// Write a single pixel. Out-of-bounds coordinates are ignored.
pub fn put_pixel(x: usize, y: usize, color: Color) {
    let fb = FB.lock();
    if let Some(fb) = fb.as_ref() {
        put_pixel_raw(fb, x, y, color);
    }
}

/// Internal: write a pixel directly (assumes lock held).
#[inline]
fn put_pixel_raw(fb: &FrameBuffer, x: usize, y: usize, color: Color) {
    let info = &fb.info;
    if x >= info.width || y >= info.height {
        return;
    }
    let bpp = info.bytes_per_pixel;
    let offset = (y * info.stride + x) * bpp;
    if offset + bpp > info.byte_len {
        return;
    }
    // SAFETY: bounds checked above; ptr is valid for byte_len bytes.
    unsafe {
        let px = fb.ptr.add(offset);
        match info.pixel_format {
            PixelFormat::Rgb => {
                *px.add(0) = color.r();
                *px.add(1) = color.g();
                *px.add(2) = color.b();
            }
            PixelFormat::Bgr => {
                *px.add(0) = color.b();
                *px.add(1) = color.g();
                *px.add(2) = color.r();
            }
            PixelFormat::U8 => {
                // Grayscale luminance approximation.
                let lum = ((color.r() as u32 * 54
                    + color.g() as u32 * 183
                    + color.b() as u32 * 19)
                    >> 8) as u8;
                *px.add(0) = lum;
            }
            _ => {
                // Unknown format: best-effort write low byte.
                *px.add(0) = color.b();
            }
        }
    }
}

/// Fill an axis-aligned rectangle with a solid color (clipped to screen).
pub fn fill_rect(x: usize, y: usize, w: usize, h: usize, color: Color) {
    let fb = FB.lock();
    if let Some(fb) = fb.as_ref() {
        let info = &fb.info;
        let x_end = core::cmp::min(x.saturating_add(w), info.width);
        let y_end = core::cmp::min(y.saturating_add(h), info.height);
        for yy in y..y_end {
            for xx in x..x_end {
                put_pixel_raw(fb, xx, yy, color);
            }
        }
    }
}

/// Draw a 1px-wide rectangle outline.
pub fn draw_rect_outline(x: usize, y: usize, w: usize, h: usize, color: Color) {
    if w == 0 || h == 0 {
        return;
    }
    fill_rect(x, y, w, 1, color); // top
    fill_rect(x, y + h - 1, w, 1, color); // bottom
    fill_rect(x, y, 1, h, color); // left
    fill_rect(x + w - 1, y, 1, h, color); // right
}

/// Clear the whole screen to a solid color.
pub fn clear(color: Color) {
    let (w, h) = {
        let fb = FB.lock();
        match fb.as_ref() {
            Some(fb) => (fb.info.width, fb.info.height),
            None => return,
        }
    };
    fill_rect(0, 0, w, h, color);
}

/// Draw one 8x8 glyph at pixel coordinates with the given fg/bg colors.
pub fn draw_glyph(ch: char, px: usize, py: usize, fg: Color, bg: Color) {
    let idx = ch as usize;
    let glyph = if idx < 128 {
        font8x8::legacy::BASIC_LEGACY[idx]
    } else {
        font8x8::legacy::BASIC_LEGACY['?' as usize]
    };
    let fb = FB.lock();
    if let Some(fb) = fb.as_ref() {
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..GLYPH_W {
                // font8x8: bit 0 (LSB) is the leftmost pixel.
                let on = (bits >> col) & 1 != 0;
                let color = if on { fg } else { bg };
                put_pixel_raw(fb, px + col, py + row, color);
            }
        }
    }
}

/// Read back the color of a single pixel. Used by the GUI cursor to save the
/// pixels it is about to overwrite so it can restore them when it moves
/// (a software "save-under" sprite). Returns [`Color::BLACK`] if out of bounds.
pub fn read_pixel(x: usize, y: usize) -> Color {
    let fb = FB.lock();
    let fb = match fb.as_ref() {
        Some(f) => f,
        None => return Color::BLACK,
    };
    let info = &fb.info;
    if x >= info.width || y >= info.height {
        return Color::BLACK;
    }
    let bpp = info.bytes_per_pixel;
    let offset = (y * info.stride + x) * bpp;
    if offset + bpp > info.byte_len {
        return Color::BLACK;
    }
    // SAFETY: bounds checked above.
    unsafe {
        let px = fb.ptr.add(offset);
        let (a, b, c) = (*px.add(0), *px.add(1), *px.add(2));
        match info.pixel_format {
            PixelFormat::Rgb => Color::rgb(a, b, c),
            PixelFormat::Bgr => Color::rgb(c, b, a),
            PixelFormat::U8 => Color::rgb(a, a, a),
            _ => Color::rgb(a, b, c),
        }
    }
}

/// Draw a glyph, leaving background pixels untouched (transparent text). Useful
/// when drawing labels over an already-painted widget.
pub fn draw_glyph_fg(ch: char, px: usize, py: usize, fg: Color) {
    let idx = ch as usize;
    let glyph = if idx < 128 {
        font8x8::legacy::BASIC_LEGACY[idx]
    } else {
        font8x8::legacy::BASIC_LEGACY['?' as usize]
    };
    let fb = FB.lock();
    if let Some(fb) = fb.as_ref() {
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..GLYPH_W {
                if (bits >> col) & 1 != 0 {
                    put_pixel_raw(fb, px + col, py + row, fg);
                }
            }
        }
    }
}

/// Draw a string of 8x8 glyphs starting at pixel `(px, py)` with a solid
/// background. Advances 8px per character; no wrapping.
pub fn draw_string(s: &str, px: usize, py: usize, fg: Color, bg: Color) {
    let mut x = px;
    for ch in s.chars() {
        draw_glyph(ch, x, py, fg, bg);
        x += GLYPH_W;
    }
}

/// Draw a string with a transparent background (only the lit pixels are drawn).
pub fn draw_string_fg(s: &str, px: usize, py: usize, fg: Color) {
    let mut x = px;
    for ch in s.chars() {
        draw_glyph_fg(ch, x, py, fg);
        x += GLYPH_W;
    }
}

/// Draw a string scaled up by an integer factor (each font pixel becomes a
/// `scale`x`scale` block). Transparent background. Lets the GUI render larger
/// titles/labels without a second font.
pub fn draw_string_scaled(s: &str, px: usize, py: usize, fg: Color, scale: usize) {
    if scale == 0 {
        return;
    }
    let mut x = px;
    for ch in s.chars() {
        let idx = ch as usize;
        let glyph = if idx < 128 {
            font8x8::legacy::BASIC_LEGACY[idx]
        } else {
            font8x8::legacy::BASIC_LEGACY['?' as usize]
        };
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..GLYPH_W {
                if (bits >> col) & 1 != 0 {
                    fill_rect(x + col * scale, py + row * scale, scale, scale, fg);
                }
            }
        }
        x += GLYPH_W * scale;
    }
}

// ---------------------------------------------------------------------------
// Text console (so `print!`/`println!` work in graphics mode).
// ---------------------------------------------------------------------------

/// True if the framebuffer text console should receive console output.
pub fn console_active() -> bool {
    CONSOLE.lock().active
}

/// When the GUI compositor owns the screen the full-screen text console must
/// stop drawing or it would scribble over the desktop. While suspended,
/// `print!`/`println!` output is redirected to the serial console instead (see
/// `vga_buffer::_print`); the framebuffer itself remains valid for GUI drawing.
static CONSOLE_SUSPENDED: AtomicBool = AtomicBool::new(false);

/// Suspend (or resume) the full-screen text console. Called by the compositor
/// when it takes over / relinquishes the display.
pub fn suspend_console(suspend: bool) {
    CONSOLE_SUSPENDED.store(suspend, Ordering::Release);
}

/// True while the text console is suspended for the GUI.
pub fn console_suspended() -> bool {
    CONSOLE_SUSPENDED.load(Ordering::Acquire)
}

/// Set the console foreground/background colors.
#[allow(dead_code)]
pub fn set_console_colors(fg: Color, bg: Color) {
    let mut con = CONSOLE.lock();
    con.fg = fg;
    con.bg = bg;
}

/// Write a string to the framebuffer text console.
pub fn console_write_str(s: &str) {
    let (fg, bg, mut col, mut row, cols, rows) = {
        let con = CONSOLE.lock();
        (con.fg, con.bg, con.col, con.row, con.cols, con.rows)
    };
    if cols == 0 || rows == 0 {
        return;
    }
    for ch in s.chars() {
        match ch {
            '\n' => {
                col = 0;
                row += 1;
            }
            '\r' => {
                col = 0;
            }
            '\t' => {
                col = (col + 4) & !3;
            }
            '\u{8}' => {
                // Backspace: erase previous cell.
                if col > 0 {
                    col -= 1;
                    draw_glyph(' ', col * GLYPH_W, row * GLYPH_H, fg, bg);
                }
            }
            c => {
                draw_glyph(c, col * GLYPH_W, row * GLYPH_H, fg, bg);
                col += 1;
            }
        }
        if col >= cols {
            col = 0;
            row += 1;
        }
        if row >= rows {
            scroll_up(bg);
            row = rows - 1;
        }
    }
    let mut con = CONSOLE.lock();
    con.col = col;
    con.row = row;
}

/// Scroll the console up by one text row (8 px), filling the new line with bg.
fn scroll_up(bg: Color) {
    let fb = FB.lock();
    if let Some(fb) = fb.as_ref() {
        let info = &fb.info;
        let bpp = info.bytes_per_pixel;
        let row_bytes = info.stride * bpp;
        let shift = GLYPH_H * row_bytes;
        let total = info.height * row_bytes;
        // SAFETY: moving within the framebuffer region; bounds respected.
        unsafe {
            // Move everything up by one glyph row.
            core::ptr::copy(fb.ptr.add(shift), fb.ptr, total - shift);
        }
        // Clear the last text row.
        let last_y = (info.height / GLYPH_H - 1) * GLYPH_H;
        for yy in last_y..info.height {
            for xx in 0..info.width {
                put_pixel_raw(fb, xx, yy, bg);
            }
        }
    }
}

/// `core::fmt::Write` shim so we can use `write_fmt`.
struct ConsoleWriter;

impl fmt::Write for ConsoleWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        console_write_str(s);
        Ok(())
    }
}

/// Print formatted args to the framebuffer console (used by `vga_buffer::_print`).
pub fn console_print(args: fmt::Arguments) {
    use core::fmt::Write;
    let _ = ConsoleWriter.write_fmt(args);
}

/// Draw a boot-time test pattern proving per-pixel graphics work: a desktop
/// background, a title bar, a few colored rectangles, and a horizontal RGB
/// gradient strip. Visible at boot before the windowing system exists.
pub fn draw_boot_test_pattern() {
    let (w, h) = (width(), height());
    if w == 0 || h == 0 {
        return;
    }

    // Desktop background.
    clear(Color::DESKTOP);

    // Top title bar.
    fill_rect(0, 0, w, 24, Color(0x123A52));
    fill_rect(0, 24, w, 2, Color(0x2BD6C4));

    // A row of demo color swatches.
    let swatches = [
        Color::RED,
        Color(0xFF8800),
        Color::YELLOW,
        Color::GREEN,
        Color::CYAN,
        Color::BLUE,
        Color::MAGENTA,
        Color::WHITE,
    ];
    let sw = 60usize;
    let sh = 60usize;
    let start_x = 40usize;
    let start_y = 60usize;
    for (i, &c) in swatches.iter().enumerate() {
        let x = start_x + i * (sw + 12);
        if x + sw > w {
            break;
        }
        fill_rect(x, start_y, sw, sh, c);
        draw_rect_outline(x, start_y, sw, sh, Color::WHITE);
    }

    // A smooth horizontal gradient strip (red -> green -> blue).
    let grad_y = start_y + sh + 40;
    let grad_h = 50usize;
    if grad_y + grad_h < h {
        for x in 0..w {
            let t = x * 255 / w.max(1);
            let (r, g, b) = if t < 128 {
                // red -> green
                let k = (t * 2) as u8;
                (255u8.saturating_sub(k), k, 0u8)
            } else {
                // green -> blue
                let k = ((t - 128) * 2) as u8;
                (0u8, 255u8.saturating_sub(k), k)
            };
            fill_rect(x, grad_y, 1, grad_h, Color::rgb(r, g, b));
        }
    }
}
