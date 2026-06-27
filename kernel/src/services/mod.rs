//! Core OS services for PICKLE OS Phase 2.
//!
//! This module groups the system services that, in a mature microkernel, run as
//! isolated user-space processes communicating purely via IPC and capabilities:
//!
//!   * [`init`]     — bootstraps the other services and the namespace.
//!   * [`registry`] — name -> endpoint directory (service discovery).
//!   * [`vfs`]      — filesystem service exposing open/read/write/readdir/stat.
//!   * [`memfs`]    — in-memory backend behind the VFS.
//!
//! In this phase the services run as in-kernel tasks (sharing the kernel address
//! space) but interact *only* through the same IPC `call`/`reply` primitives a
//! user process would use. This keeps the protocol and component boundaries
//! honest, so migrating a service into its own address space later is a matter
//! of changing where the task runs — not how it communicates.

pub mod init;
pub mod memfs;
pub mod registry;
pub mod vfs;

/// Start the Phase 2 service stack by spawning the Init server, which in turn
/// brings up the Registry and VFS servers.
pub fn start() {
    crate::task::spawn_kernel_task("init", init::init_server_task);
}

/// End-to-end self-test of the service stack, run once at boot. It acts purely
/// as a VFS *client* (everything goes through IPC), so a successful run proves
/// the shell -> VFS -> MemFS path works without needing keyboard input. Results
/// are logged to the serial console.
pub extern "C" fn vfs_selftest_task() -> ! {
    use crate::serial_println;

    serial_println!("[selftest] === VFS over IPC self-test ===");

    // 1. Resolve the VFS service through the Registry (service discovery).
    match registry::lookup(vfs::VFS_ENDPOINT) {
        Some(ep) => serial_println!("[selftest] registry resolved 'vfs' -> endpoint {}", ep),
        None => serial_println!("[selftest] WARN: 'vfs' not in registry"),
    }

    // 2. List the seeded root directory.
    match vfs::readdir("/") {
        Ok(entries) => serial_println!("[selftest] ls / -> {:?}", entries),
        Err(e) => serial_println!("[selftest] ls / FAILED: {:?}", e),
    }

    // 3. Read a seed file.
    match vfs::read("/welcome.txt") {
        Ok(data) => serial_println!(
            "[selftest] cat /welcome.txt -> {:?}",
            core::str::from_utf8(&data).unwrap_or("<binary>")
        ),
        Err(e) => serial_println!("[selftest] cat /welcome.txt FAILED: {:?}", e),
    }

    // 4. Create a directory and a file, write to it, read it back.
    let _ = vfs::mkdir("/tmp");
    match vfs::write("/tmp/hello.txt", b"hello from the VFS self-test\n") {
        Ok(n) => serial_println!("[selftest] write /tmp/hello.txt -> {} bytes", n),
        Err(e) => serial_println!("[selftest] write FAILED: {:?}", e),
    }
    match vfs::read("/tmp/hello.txt") {
        Ok(data) => serial_println!(
            "[selftest] read back /tmp/hello.txt -> {:?}",
            core::str::from_utf8(&data).unwrap_or("<binary>")
        ),
        Err(e) => serial_println!("[selftest] read back FAILED: {:?}", e),
    }

    // 5. Stat it, list /tmp, then remove it and confirm.
    match vfs::stat("/tmp/hello.txt") {
        Ok(info) => serial_println!(
            "[selftest] stat /tmp/hello.txt -> size={} is_dir={}",
            info.size,
            info.is_dir
        ),
        Err(e) => serial_println!("[selftest] stat FAILED: {:?}", e),
    }
    match vfs::readdir("/tmp") {
        Ok(entries) => serial_println!("[selftest] ls /tmp -> {:?}", entries),
        Err(e) => serial_println!("[selftest] ls /tmp FAILED: {:?}", e),
    }
    match vfs::remove("/tmp/hello.txt") {
        Ok(()) => serial_println!("[selftest] rm /tmp/hello.txt -> ok"),
        Err(e) => serial_println!("[selftest] rm FAILED: {:?}", e),
    }
    match vfs::read("/tmp/hello.txt") {
        Ok(_) => serial_println!("[selftest] ERROR: file still readable after rm"),
        Err(e) => serial_println!("[selftest] confirmed removed ({:?})", e),
    }

    serial_println!("[selftest] === self-test complete ===");

    // Done; idle out so we don't spin.
    loop {
        crate::task::yield_now();
    }
}
