//! PS/2 mouse driver — a real device driver running as an ordinary task.
//!
//! Mirrors the keyboard driver ([`crate::driver::keyboard`]): the kernel's
//! first-level IRQ12 handler only acknowledges the PICs and notifies this task,
//! which then reads the PS/2 data port (0x60) through capability-checked
//! [`crate::driver::portio`] accessors and decodes the 3-byte movement packets.
//!
//! The decoded pointer position + button state is published in [`STATE`], a
//! shared snapshot the GUI compositor ([`crate::gui`]) polls each frame to move
//! the cursor and drag windows.
//!
//! ## PS/2 mouse protocol
//! The 8042 controller multiplexes the keyboard and the "auxiliary" device
//! (mouse) on the same data port (0x60); IRQ1 vs IRQ12 disambiguates them.
//! Each mouse report is three bytes:
//!   * byte0 — flags: bit0 L, bit1 R, bit2 M, bit3 always-1, bit4 X sign,
//!     bit5 Y sign, bit6 X overflow, bit7 Y overflow;
//!   * byte1 — signed X delta (sign extended from byte0 bit4);
//!   * byte2 — signed Y delta (sign extended from byte0 bit5; +Y is up).

use crate::capability::{self, Object, Rights};
use crate::driver::{irq, portio};
use crate::serial_println;
use crate::task;
use spin::Mutex;

/// IRQ line and I/O ports for the legacy PS/2 mouse (auxiliary device).
const MOUSE_IRQ: u8 = 12;
const PS2_DATA: u16 = 0x60; // data register (read/write device bytes)
const PS2_CMD: u16 = 0x64; // status (read) / command (write) register

/// A snapshot of the pointer state, published for the GUI compositor.
#[derive(Clone, Copy)]
pub struct MouseState {
    /// Absolute cursor position, clamped to the screen.
    pub x: i32,
    pub y: i32,
    pub left: bool,
    pub right: bool,
    pub middle: bool,
    /// Monotonic counter bumped on every processed packet, so the compositor
    /// can cheaply tell whether anything changed since the last frame.
    pub seq: u64,
}

impl MouseState {
    const fn new() -> Self {
        MouseState {
            x: 0,
            y: 0,
            left: false,
            right: false,
            middle: false,
            seq: 0,
        }
    }
}

static STATE: Mutex<MouseState> = Mutex::new(MouseState::new());

/// Screen bounds the cursor is clamped to. Set once the framebuffer is known.
static BOUNDS: Mutex<(i32, i32)> = Mutex::new((1024, 768));

/// Read the current pointer snapshot (cheap; copies out under a brief lock).
pub fn state() -> MouseState {
    *STATE.lock()
}

/// Seed the cursor position (usually the screen centre) and the clamp bounds.
pub fn set_bounds_and_center(width: i32, height: i32) {
    *BOUNDS.lock() = (width, height);
    let mut s = STATE.lock();
    s.x = width / 2;
    s.y = height / 2;
}

// --- Low-level 8042 controller helpers -------------------------------------

/// Wait until the controller's input buffer is empty (safe to write a byte).
fn wait_write() {
    for _ in 0..100_000 {
        match portio::inb(PS2_CMD) {
            Ok(st) if st & 0x02 == 0 => return,
            _ => core::hint::spin_loop(),
        }
    }
}

/// Wait until the controller's output buffer is full (a byte is ready to read).
fn wait_read() {
    for _ in 0..100_000 {
        match portio::inb(PS2_CMD) {
            Ok(st) if st & 0x01 != 0 => return,
            _ => core::hint::spin_loop(),
        }
    }
}

/// Send a command byte to the mouse (auxiliary device) and read its ACK.
fn mouse_command(cmd: u8) {
    wait_write();
    let _ = portio::outb(PS2_CMD, 0xD4); // "next byte goes to the mouse"
    wait_write();
    let _ = portio::outb(PS2_DATA, cmd);
    // Consume the 0xFA ACK.
    wait_read();
    let _ = portio::inb(PS2_DATA);
}

/// Initialise the 8042 auxiliary port and the mouse: enable the device, turn on
/// IRQ12 in the controller config byte, then put the mouse in streaming mode.
fn init_controller() {
    // Enable the auxiliary (mouse) PS/2 port.
    wait_write();
    let _ = portio::outb(PS2_CMD, 0xA8);

    // Read the controller configuration byte (command 0x20).
    wait_write();
    let _ = portio::outb(PS2_CMD, 0x20);
    wait_read();
    let mut config = portio::inb(PS2_DATA).unwrap_or(0);
    config |= 0x02; // bit1: enable IRQ12 (mouse interrupt)
    config &= !0x20; // bit5: clear "disable mouse clock" -> mouse clock enabled

    // Write the configuration byte back (command 0x60).
    wait_write();
    let _ = portio::outb(PS2_CMD, 0x60);
    wait_write();
    let _ = portio::outb(PS2_DATA, config);

    // Tell the mouse to use defaults, then enable data reporting (streaming).
    mouse_command(0xF6); // set defaults
    mouse_command(0xF4); // enable packet streaming
}

/// Apply a freshly assembled 3-byte packet to the shared pointer state.
fn apply_packet(packet: [u8; 3]) {
    let flags = packet[0];
    // Discard packets with the overflow bits set (corrupt movement).
    if flags & 0xC0 != 0 {
        return;
    }
    // 9-bit signed deltas: sign bit lives in the flags byte.
    let mut dx = packet[1] as i32;
    let mut dy = packet[2] as i32;
    if flags & 0x10 != 0 {
        dx -= 0x100;
    }
    if flags & 0x20 != 0 {
        dy -= 0x100;
    }

    let (w, h) = *BOUNDS.lock();
    let mut s = STATE.lock();
    s.x = (s.x + dx).clamp(0, w - 1);
    // PS/2 reports +Y as up; screen Y grows downward, so subtract.
    s.y = (s.y - dy).clamp(0, h - 1);
    s.left = flags & 0x01 != 0;
    s.right = flags & 0x02 != 0;
    s.middle = flags & 0x04 != 0;
    s.seq = s.seq.wrapping_add(1);
}

/// The mouse driver task entry point.
pub extern "C" fn mouse_driver_task() -> ! {
    let me = task::current_id();

    // Grant ourselves the capabilities a mouse driver needs: IRQ12 plus the
    // PS/2 data + command/status ports (0x60..0x64).
    capability::mint(me, Object::Irq(MOUSE_IRQ), Rights::ALL);
    capability::mint(
        me,
        Object::Port {
            base: PS2_DATA,
            count: 5, // 0x60..=0x64
        },
        Rights::READ.union(Rights::WRITE),
    );

    if !irq::register(MOUSE_IRQ, me) {
        serial_println!("[mouse] ERROR: IRQ{} already owned", MOUSE_IRQ);
    }

    init_controller();
    serial_println!(
        "[mouse] PS/2 mouse driver online (owns IRQ{} + ports {:#x}..{:#x})",
        MOUSE_IRQ,
        PS2_DATA,
        PS2_CMD
    );

    // Packet assembly state. We resync on the always-1 bit of byte0.
    let mut packet = [0u8; 3];
    let mut index = 0usize;

    loop {
        // Block until the kernel signals a mouse interrupt.
        let _pending = irq::wait(MOUSE_IRQ);

        // Drain every byte currently available from the controller. Each IRQ12
        // may carry one or more bytes depending on timing.
        loop {
            let status = match portio::inb(PS2_CMD) {
                Ok(s) => s,
                Err(_) => break,
            };
            // bit0: output buffer full; bit5: byte came from the aux (mouse).
            if status & 0x01 == 0 {
                break;
            }
            let byte = match portio::inb(PS2_DATA) {
                Ok(b) => b,
                Err(_) => break,
            };
            if status & 0x20 == 0 {
                // Not mouse data (stray keyboard byte) — ignore.
                continue;
            }

            // Resync: the first byte of a packet always has bit3 set.
            if index == 0 && byte & 0x08 == 0 {
                continue;
            }
            packet[index] = byte;
            index += 1;
            if index == 3 {
                apply_packet(packet);
                index = 0;
            }
        }
    }
}
