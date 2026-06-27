//! PICKLE OS User Library (libpickleos)
//!
//! A minimal, no_std Rust library for user-space programs on PICKLE OS.
//! Provides syscall wrappers, IPC abstractions, and basic utilities.

#![no_std]
#![feature(alloc_error_handler)]

extern crate alloc;

pub mod syscall;
pub mod ipc;
pub mod capability;
pub mod process;
pub mod memory;
pub mod gui;
pub mod widgets;
pub mod socket;
pub mod font;

use core::panic::PanicInfo;

/// Panic handler for user-space programs
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Print panic info via syscall
    syscall::sys_print("PANIC: ");
    
    if let Some(location) = info.location() {
        syscall::sys_print("at ");
        syscall::sys_print(location.file());
        syscall::sys_print(":");
        // Can't format line number without heap potentially being broken
        syscall::sys_print("<line>");
        syscall::sys_print(": ");
    }
    
    // Print the panic message
    let _message = info.message();
    syscall::sys_print("<panic occurred>\n");
    
    syscall::sys_exit(1);
}

/// Allocation error handler
#[alloc_error_handler]
fn alloc_error_handler(_layout: core::alloc::Layout) -> ! {
    syscall::sys_print("Out of memory: allocation failed\n");
    syscall::sys_exit(1);
}

/// Initialize the user-space heap allocator.
/// Must be called before using any heap-allocated types (Vec, String, etc.).
pub fn init_heap() -> Result<(), &'static str> {
    use memory::HEAP_START;
    const HEAP_SIZE: usize = 1024 * 1024; // 1 MiB
    
    // Use mmap to allocate heap memory
    let heap_addr = syscall::sys_mmap(HEAP_START as u64, HEAP_SIZE, 0x3)?;
    
    unsafe {
        memory::ALLOCATOR.lock().init(heap_addr as *mut u8, HEAP_SIZE);
    }
    
    Ok(())
}

/// Print a formatted string to the console
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use alloc::format;
        let s = format!($($arg)*);
        $crate::syscall::sys_print(&s);
    }};
}

/// Print a formatted string with newline to the console
#[macro_export]
macro_rules! println {
    () => { $crate::syscall::sys_print("\n") };
    ($($arg:tt)*) => {{
        use alloc::format;
        let s = format!($($arg)*);
        $crate::syscall::sys_print(&s);
        $crate::syscall::sys_print("\n");
    }};
}
