//! Capability-checked x86 port I/O for driver tasks.
//!
//! Legacy devices (PS/2 keyboard, PIT, serial UART, ...) are driven through the
//! processor's I/O ports. Letting any task execute `in`/`out` would defeat
//! isolation, so a driver must first hold an `Object::Port { base, count }`
//! capability covering the port it wants to touch. These wrappers verify that
//! the calling task holds such a capability before performing the access.
//!
//! In this phase the driver tasks run in the kernel address space, so the
//! `in`/`out` instructions execute directly after the capability check. When
//! drivers move to ring 3, the same capability check will gate an I/O-port
//! bitmap in the TSS (or an `mmap`-style grant) instead — the *authorization
//! model* is identical, only the enforcement mechanism changes.

use crate::capability::{self, Object, Rights};
use x86_64::instructions::port::Port;

/// Does `task_id` hold a Port capability covering `[port, port+width)`?
fn task_owns_port(task_id: u64, port: u16, width: u16) -> bool {
    capability::find_object(task_id, Rights::READ.union(Rights::WRITE), |obj| {
        if let Object::Port { base, count } = obj {
            port >= *base && (port as u32 + width as u32) <= (*base as u32 + *count as u32)
        } else {
            false
        }
    })
    .is_some()
}

/// Read a byte from `port`, if the current task holds a covering Port capability.
pub fn inb(port: u16) -> Result<u8, &'static str> {
    let me = crate::task::current_id();
    if !task_owns_port(me, port, 1) {
        return Err("port-io: missing Port capability");
    }
    let mut p: Port<u8> = Port::new(port);
    Ok(unsafe { p.read() })
}

/// Write a byte to `port`, if the current task holds a covering Port capability.
pub fn outb(port: u16, value: u8) -> Result<(), &'static str> {
    let me = crate::task::current_id();
    if !task_owns_port(me, port, 1) {
        return Err("port-io: missing Port capability");
    }
    let mut p: Port<u8> = Port::new(port);
    unsafe { p.write(value) };
    Ok(())
}
