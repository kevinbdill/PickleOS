//! PS/2 keyboard driver — a real device driver running as an ordinary task.
//!
//! This is the canonical example of the PICKLE OS driver model: instead of the
//! kernel reading the keyboard inside its interrupt handler, a dedicated driver
//! *task* owns IRQ1 and the PS/2 data port (0x60), both granted as
//! capabilities. The kernel's first-level IRQ1 handler does nothing but
//! acknowledge the PIC and notify this task (see `interrupts.rs` +
//! [`crate::driver::irq`]).
//!
//! Flow per keystroke:
//!   1. Hardware raises IRQ1.
//!   2. Kernel handler acks the PIC and calls `irq::notify_from_isr(1)`.
//!   3. This task wakes from `irq::wait(1)`.
//!   4. It reads the scancode from port 0x60 via the capability-checked
//!      [`crate::driver::portio`] wrapper.
//!   5. It forwards the scancode to the shell's input queue.
//!
//! All device-touching work happens here at task priority, fully preemptible —
//! exactly the property that makes microkernel drivers robust.

use crate::capability::{self, Object, Rights};
use crate::driver::{irq, portio};
use crate::serial_println;
use crate::task;

/// IRQ line and data port for the legacy PS/2 keyboard.
const KEYBOARD_IRQ: u8 = 1;
const PS2_DATA_PORT: u16 = 0x60; // data register
const PS2_CMD_PORT: u16 = 0x64; // status (read) / command (write) register

/// Busy-wait until the 8042 input buffer is empty (safe to write a byte).
/// Bounded so a wedged controller can never hang the boot.
fn wait_write() {
    for _ in 0..100_000 {
        if portio::inb(PS2_CMD_PORT).unwrap_or(0xFF) & 0x02 == 0 {
            return;
        }
    }
}

/// Busy-wait until the 8042 output buffer is full (a byte is ready to read).
fn wait_read() {
    for _ in 0..100_000 {
        if portio::inb(PS2_CMD_PORT).unwrap_or(0) & 0x01 != 0 {
            return;
        }
    }
}

/// Initialise the legacy 8042 keyboard controller so the keyboard actually
/// generates interrupts. Some firmware leaves the first PS/2 port with its
/// clock disabled or its IRQ masked; without this, QEMU `-display none` boots
/// hand us a keyboard that never raises IRQ1. We run this *before* the mouse
/// driver touches the shared config byte, and use busy-polls (no task yields)
/// so the whole sequence completes atomically with respect to the mouse task.
fn init_controller() {
    // Drain any stale bytes sitting in the output buffer.
    for _ in 0..16 {
        if portio::inb(PS2_CMD_PORT).unwrap_or(0) & 0x01 == 0 {
            break;
        }
        let _ = portio::inb(PS2_DATA_PORT);
    }

    // Read the controller configuration byte (command 0x20).
    wait_write();
    let _ = portio::outb(PS2_CMD_PORT, 0x20);
    wait_read();
    let mut config = portio::inb(PS2_DATA_PORT).unwrap_or(0);
    config |= 0x01; // bit0: enable keyboard (port 1) interrupt -> IRQ1
    config &= !0x10; // bit4: clear "disable keyboard clock" -> clock enabled

    // Write the configuration byte back (command 0x60).
    wait_write();
    let _ = portio::outb(PS2_CMD_PORT, 0x60);
    wait_write();
    let _ = portio::outb(PS2_DATA_PORT, config);

    // Tell the keyboard device itself to (re)enable scanning (command 0xF4),
    // then swallow its ACK (0xFA).
    wait_write();
    let _ = portio::outb(PS2_DATA_PORT, 0xF4);
    wait_read();
    let _ = portio::inb(PS2_DATA_PORT);
}

/// The keyboard driver task entry point.
pub extern "C" fn keyboard_driver_task() -> ! {
    let me = task::current_id();

    // Grant ourselves the capabilities a keyboard driver needs. In a fully
    // fledged system Init would mint and hand these to us; here the kernel
    // bootstraps them directly, but every subsequent access is capability-
    // checked exactly as a ring-3 driver's would be.
    capability::mint(me, Object::Irq(KEYBOARD_IRQ), Rights::ALL);
    capability::mint(
        me,
        Object::Port {
            base: PS2_DATA_PORT,
            count: 5, // 0x60..=0x64 (data + status/command register)
        },
        Rights::READ.union(Rights::WRITE),
    );

    // Bring the 8042 up so the keyboard actually raises IRQ1 (must happen
    // before the mouse driver rewrites the shared controller config byte).
    init_controller();

    // Claim ownership of IRQ1 so the bridge will route notifications to us.
    if !irq::register(KEYBOARD_IRQ, me) {
        serial_println!("[kbd] ERROR: IRQ{} already owned", KEYBOARD_IRQ);
    }
    serial_println!("[kbd] keyboard driver online (owns IRQ{} + ports {:#x}..{:#x})", KEYBOARD_IRQ, PS2_DATA_PORT, PS2_CMD_PORT);

    loop {
        // Block until the kernel signals a keyboard interrupt.
        let _pending = irq::wait(KEYBOARD_IRQ);

        // Drain the PS/2 output buffer. Each IRQ usually corresponds to one
        // byte, but draining defensively avoids a stuck buffer.
        match portio::inb(PS2_DATA_PORT) {
            Ok(scancode) => {
                // Hand the raw scancode to the console line discipline, which
                // owns decoding and feeds both the shell and stdin readers.
                crate::driver::console::feed_scancode(scancode);
            }
            Err(e) => serial_println!("[kbd] port read denied: {}", e),
        }
    }
}
