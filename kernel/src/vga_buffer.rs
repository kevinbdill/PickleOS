//! VGA text-mode console.
//!
//! On BIOS boot the screen is in 80x25 16-color text mode, backed by a memory
//! mapped buffer at physical address `0xb8000`. Each character cell is two
//! bytes: an ASCII code point and an attribute byte (foreground/background
//! color). Writing to this buffer immediately changes what's on screen.
//!
//! We wrap the buffer in a [`Writer`] that implements [`core::fmt::Write`], so
//! the `print!`/`println!` macros work just like on a hosted platform.

use alloc::collections::BTreeMap;
use alloc::string::String;
use core::fmt;
use lazy_static::lazy_static;
use spin::Mutex;
use volatile::Volatile;

/// Standard VGA text buffer dimensions.
const BUFFER_HEIGHT: usize = 25;
const BUFFER_WIDTH: usize = 80;

/// The 16 VGA colors. The attribute byte packs a foreground color (low nibble)
/// and a background color (high nibble).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Color {
    Black = 0,
    Blue = 1,
    Green = 2,
    Cyan = 3,
    Red = 4,
    Magenta = 5,
    Brown = 6,
    LightGray = 7,
    DarkGray = 8,
    LightBlue = 9,
    LightGreen = 10,
    LightCyan = 11,
    LightRed = 12,
    Pink = 13,
    Yellow = 14,
    White = 15,
}

/// A packed foreground/background color attribute byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
struct ColorCode(u8);

impl ColorCode {
    fn new(foreground: Color, background: Color) -> ColorCode {
        ColorCode((background as u8) << 4 | (foreground as u8))
    }
}

/// One character cell in the VGA buffer: glyph + color attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
struct ScreenChar {
    ascii_character: u8,
    color_code: ColorCode,
}

/// The memory-mapped VGA buffer. `Volatile` prevents the optimizer from eliding
/// writes it thinks are dead (the compiler can't see the screen reads them).
#[repr(transparent)]
struct Buffer {
    chars: [[Volatile<ScreenChar>; BUFFER_WIDTH]; BUFFER_HEIGHT],
}

/// Stateful writer that tracks the cursor column and current color, and knows
/// how to scroll the screen.
pub struct Writer {
    column_position: usize,
    color_code: ColorCode,
    buffer: &'static mut Buffer,
}

impl Writer {
    /// Write a single byte, handling newlines and line wrapping + scrolling.
    pub fn write_byte(&mut self, byte: u8) {
        match byte {
            b'\n' => self.new_line(),
            byte => {
                if self.column_position >= BUFFER_WIDTH {
                    self.new_line();
                }
                let row = BUFFER_HEIGHT - 1;
                let col = self.column_position;
                let color_code = self.color_code;
                self.buffer.chars[row][col].write(ScreenChar {
                    ascii_character: byte,
                    color_code,
                });
                self.column_position += 1;
            }
        }
    }

    /// Write a string, substituting a `■` for any non-printable byte so the
    /// console never emits garbage outside the printable VGA range.
    pub fn write_string(&mut self, s: &str) {
        for byte in s.bytes() {
            match byte {
                0x20..=0x7e | b'\n' => self.write_byte(byte),
                _ => self.write_byte(0xfe),
            }
        }
    }

    /// Set the active color for subsequently written characters.
    pub fn set_color(&mut self, fg: Color, bg: Color) {
        self.color_code = ColorCode::new(fg, bg);
    }

    /// Advance to a new line, scrolling everything up by one row when the
    /// cursor reaches the bottom of the screen.
    fn new_line(&mut self) {
        for row in 1..BUFFER_HEIGHT {
            for col in 0..BUFFER_WIDTH {
                let character = self.buffer.chars[row][col].read();
                self.buffer.chars[row - 1][col].write(character);
            }
        }
        self.clear_row(BUFFER_HEIGHT - 1);
        self.column_position = 0;
    }

    /// Blank a single row using the current color.
    fn clear_row(&mut self, row: usize) {
        let blank = ScreenChar {
            ascii_character: b' ',
            color_code: self.color_code,
        };
        for col in 0..BUFFER_WIDTH {
            self.buffer.chars[row][col].write(blank);
        }
    }

    /// Clear the whole screen.
    pub fn clear_screen(&mut self) {
        for row in 0..BUFFER_HEIGHT {
            self.clear_row(row);
        }
        self.column_position = 0;
    }
}

impl fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_string(s);
        Ok(())
    }
}

lazy_static! {
    /// The global VGA writer. Guarded by a spinlock for safe shared access.
    pub static ref WRITER: Mutex<Writer> = Mutex::new(Writer {
        column_position: 0,
        color_code: ColorCode::new(Color::LightGreen, Color::Black),
        // SAFETY: 0xb8000 is the fixed VGA text buffer; we have exclusive use.
        buffer: unsafe { &mut *(0xb8000 as *mut Buffer) },
    });
}

/// Per-task output-capture buffers. When the *currently running* task has an
/// entry here, console output produced via the `print!`/`println!` macros is
/// appended to it *instead* of being drawn to the screen. The in-kernel shell
/// uses this to implement output redirection (`cmd > file`) and pipelines
/// (`cmd1 | cmd2`).
///
/// Keying capture by task id (rather than a single global buffer) keeps several
/// shells' captures independent and, crucially, prevents output produced by an
/// *unrelated* task that happens to run while a capturing command has yielded
/// (e.g. a disk-reading filter waiting on AHCI) from leaking into the capture.
static CAPTURE: Mutex<BTreeMap<u64, String>> = Mutex::new(BTreeMap::new());

/// Begin capturing the current task's console output (replaces any previous
/// capture buffer for this task).
pub fn capture_begin() {
    let tid = crate::task::current_id_fast();
    x86_64::instructions::interrupts::without_interrupts(|| {
        CAPTURE.lock().insert(tid, String::new());
    });
}

/// Stop capturing for the current task and return whatever was collected.
pub fn capture_end() -> Option<String> {
    let tid = crate::task::current_id_fast();
    x86_64::instructions::interrupts::without_interrupts(|| CAPTURE.lock().remove(&tid))
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use core::fmt::Write;
    // Hold the lock with interrupts disabled to prevent deadlock with handlers
    // that may also print.
    x86_64::instructions::interrupts::without_interrupts(|| {
        // If a capture is active for the running task, redirect output into its
        // buffer and suppress the on-screen write (shell redirection semantics).
        let tid = crate::task::current_id_fast();
        let mut cap = CAPTURE.lock();
        if let Some(buf) = cap.get_mut(&tid) {
            let _ = buf.write_fmt(args);
            return;
        }
        drop(cap);
        // In graphics mode the legacy 0xb8000 text buffer is unavailable (and
        // may be unmapped — touching it would fault), so route console output
        // to the framebuffer text console instead.
        if crate::framebuffer::console_active() {
            if crate::framebuffer::console_suspended() {
                // The GUI compositor owns the screen. If the graphical Terminal
                // window is up, feed output into its text grid so it shows up
                // on screen; otherwise fall back to the serial port. Mirror to
                // serial regardless so logs stay visible to a headless host.
                if crate::terminal::is_active() {
                    let mut s = String::new();
                    if s.write_fmt(args).is_ok() {
                        crate::terminal::write_for_current(&s);
                        crate::serial::_print(format_args!("{}", s));
                        return;
                    }
                }
                crate::serial::_print(args);
            } else {
                crate::framebuffer::console_print(args);
            }
        } else {
            WRITER.lock().write_fmt(args).unwrap();
        }
    });
}

/// Print to the VGA console (no newline).
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::vga_buffer::_print(format_args!($($arg)*)));
}

/// Print to the VGA console (with newline).
#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}
