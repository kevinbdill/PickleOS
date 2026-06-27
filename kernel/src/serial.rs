//! 16550 UART serial driver.
//!
//! The serial port is the kernel's most reliable debug channel: QEMU redirects
//! COM1 (I/O port `0x3F8`) to the host terminal (`-serial stdio`), so anything
//! written here appears in the terminal that launched QEMU — even before the
//! VGA console is usable, and even on real hardware over a null-modem cable.

use core::fmt::{self, Write};
use lazy_static::lazy_static;
use spin::Mutex;
use uart_16550::SerialPort;

lazy_static! {
    /// Global COM1 instance, guarded by a spinlock so it is safe to use from
    /// any context (including, carefully, interrupt handlers).
    pub static ref SERIAL1: Mutex<SerialPort> = {
        // SAFETY: 0x3F8 is the standard COM1 base port on x86 PCs.
        let mut serial_port = unsafe { SerialPort::new(0x3F8) };
        serial_port.init();
        Mutex::new(serial_port)
    };
}

/// Internal helper used by the `serial_print!` macros.
#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    // Disable interrupts while holding the serial lock to avoid a deadlock if
    // an interrupt handler also tries to print to serial.
    x86_64::instructions::interrupts::without_interrupts(|| {
        SERIAL1
            .lock()
            .write_fmt(args)
            .expect("printing to serial failed");
    });
}

/// Print to the host via the serial port (no newline).
#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => {
        $crate::serial::_print(format_args!($($arg)*))
    };
}

/// Print to the host via the serial port (with newline).
#[macro_export]
macro_rules! serial_println {
    () => ($crate::serial_print!("\n"));
    ($fmt:expr) => ($crate::serial_print!(concat!($fmt, "\n")));
    ($fmt:expr, $($arg:tt)*) => ($crate::serial_print!(concat!($fmt, "\n"), $($arg)*));
}
