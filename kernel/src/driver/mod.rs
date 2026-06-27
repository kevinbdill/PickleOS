//! User-space-style device driver framework (Phase 3 foundation).
//!
//! PICKLE OS drivers are not part of the kernel. They are ordinary, preemptible,
//! capability-confined tasks that:
//!
//!   * receive hardware interrupts as notifications over the [`irq`] bridge
//!     (the kernel's first-level handler only acks the PIC and wakes the task);
//!   * touch device registers only through capability-checked [`portio`]
//!     accessors (a task must hold an `Object::Port` capability for the range);
//!   * will, in a later step, run in their own ring-3 address spaces — the
//!     isolation plumbing (per-process page tables) is already in place.
//!
//! This module provides the framework plus two concrete drivers that exercise
//! it end to end:
//!   * [`keyboard`] — a real PS/2 keyboard driver owning IRQ1 and port 0x60;
//!   * [`timer`] — a monitor that receives IRQ0 notifications alongside the
//!     kernel scheduler, proving fan-out of a kernel-shared IRQ to a task.

pub mod ahci;
pub mod block;
pub mod console;
pub mod dma;
pub mod e1000;
pub mod irq;
pub mod keyboard;
pub mod mmio;
pub mod mouse;
pub mod pci;
pub mod portio;
pub mod timer;

/// Spawn the built-in device drivers as tasks.
pub fn start() {
    crate::task::spawn_kernel_task("timer-drv", timer::timer_driver_task);
    crate::task::spawn_kernel_task("kbd", keyboard::keyboard_driver_task);
    crate::task::spawn_kernel_task("mouse", mouse::mouse_driver_task);
    // Initialize AHCI (deferred to a task context so it can mint capabilities).
    crate::task::spawn_kernel_task("ahci-init", ahci_init_task);
}

/// One-shot task to initialize the AHCI driver. Runs once, then exits.
extern "C" fn ahci_init_task() -> ! {
    ahci::init();
    // Promote discovered AHCI SATA ports to named block devices (sata0, sata1, …).
    block::init();
    // Boot-time verification: non-destructive read/write round-trip on a scratch
    // LBA of the first block device (if any). Logs PASS/FAIL to the serial console.
    if block::device_count() > 0 {
        let _ = block::selftest(0, 2048);
    }

    // NextFS bring-up (if we have a second block device). To make the
    // filesystem *persistent across reboots* we first try to mount whatever is
    // already on disk; only when no valid NextFS is found (e.g. a blank disk on
    // first boot) do we fall back to formatting it via the destructive
    // boot-time self-test. Either path leaves NextFS mounted globally.
    if block::device_count() >= 2 {
        match crate::fs::mount(1) {
            Ok(()) => {
                crate::serial_println!(
                    "[nextfs] mounted existing persistent filesystem on device 1"
                );
            }
            Err(_) => {
                crate::serial_println!(
                    "[nextfs] no existing filesystem on device 1; formatting (first boot)"
                );
                nextfs_selftest();
            }
        }

        // If the global mount succeeded, hand off to user-space init, which
        // seeds /bin + /etc/inittab and launches programs straight from disk.
        let mounted = crate::fs::with_fs(|f| f.is_some());
        if mounted {
            crate::init_user::run();
        } else {
            crate::serial_println!("[driver] NextFS not mounted; skipping user-space init");
        }
    }

    crate::serial_println!("[driver] ahci + block initialization task complete, exiting");
    // Exit the task (mark as Dead and reschedule).
    x86_64::instructions::interrupts::disable();
    crate::task::scheduler::with(|s| {
        let cur = s.current;
        s.tasks[cur].state = crate::task::State::Dead;
    });
    crate::task::scheduler::schedule();
    unreachable!("ahci_init_task returned after exit");
}

/// Boot-time NextFS functional test: format, mount, create files/dirs, read back.
fn nextfs_selftest() {
    use crate::fs::{NextFS, ROOT_INODE};
    crate::serial_println!("[nextfs] === boot-time self-test ===");

    // Test on device 1 (second disk).
    let dev_idx = 1;

    // 1. Format the device.
    if let Err(e) = NextFS::format(dev_idx) {
        crate::serial_println!("[nextfs] SELFTEST FAILED: format error: {}", e);
        return;
    }
    crate::serial_println!("[nextfs] format: OK");

    // 2. Mount it.
    let mut fs = match NextFS::mount(dev_idx) {
        Ok(fs) => fs,
        Err(e) => {
            crate::serial_println!("[nextfs] SELFTEST FAILED: mount error: {}", e);
            return;
        }
    };
    crate::serial_println!("[nextfs] mount: OK");

    // 3. List root directory (should have "." and "..").
    match fs.dir_list(ROOT_INODE) {
        Ok(entries) => {
            if entries.len() != 2 {
                crate::serial_println!("[nextfs] SELFTEST FAILED: root should have 2 entries, got {}", entries.len());
                return;
            }
            crate::serial_println!("[nextfs] root dir: OK ({} entries)", entries.len());
        }
        Err(e) => {
            crate::serial_println!("[nextfs] SELFTEST FAILED: dir_list error: {}", e);
            return;
        }
    }

    // 4. Create a directory "/test".
    let test_dir = match fs.create_dir(ROOT_INODE, "test") {
        Ok(inode) => inode,
        Err(e) => {
            crate::serial_println!("[nextfs] SELFTEST FAILED: create_dir error: {}", e);
            return;
        }
    };
    crate::serial_println!("[nextfs] create_dir /test: OK (inode {})", test_dir);

    // 5. Create a file "/test/hello.txt".
    let file_inode = match fs.create_file(test_dir, "hello.txt") {
        Ok(inode) => inode,
        Err(e) => {
            crate::serial_println!("[nextfs] SELFTEST FAILED: create_file error: {}", e);
            return;
        }
    };
    crate::serial_println!("[nextfs] create_file /test/hello.txt: OK (inode {})", file_inode);

    // 6. Write data to the file.
    let test_data = b"Hello from NextFS!\nThis is a test file.\n";
    if let Err(e) = fs.write_file(file_inode, test_data) {
        crate::serial_println!("[nextfs] SELFTEST FAILED: write_file error: {}", e);
        return;
    }
    crate::serial_println!("[nextfs] write_file: OK ({} bytes)", test_data.len());

    // 7. Read the file back and verify.
    match fs.read_file(file_inode) {
        Ok(contents) => {
            if contents.as_slice() != test_data {
                crate::serial_println!("[nextfs] SELFTEST FAILED: read_file data mismatch");
                return;
            }
            crate::serial_println!("[nextfs] read_file: OK (verified {} bytes)", contents.len());
        }
        Err(e) => {
            crate::serial_println!("[nextfs] SELFTEST FAILED: read_file error: {}", e);
            return;
        }
    }

    // 8. List /test directory (should have ".", "..", "hello.txt").
    match fs.dir_list(test_dir) {
        Ok(entries) => {
            if entries.len() != 3 {
                crate::serial_println!("[nextfs] SELFTEST FAILED: /test should have 3 entries, got {}", entries.len());
                return;
            }
            crate::serial_println!("[nextfs] /test dir: OK ({} entries)", entries.len());
        }
        Err(e) => {
            crate::serial_println!("[nextfs] SELFTEST FAILED: dir_list /test error: {}", e);
            return;
        }
    }

    // 9. Test truncate: shrink the file to 10 bytes.
    if let Err(e) = fs.truncate(file_inode, 10) {
        crate::serial_println!("[nextfs] SELFTEST FAILED: truncate error: {}", e);
        return;
    }
    match fs.read_file(file_inode) {
        Ok(contents) => {
            if contents.len() != 10 {
                crate::serial_println!("[nextfs] SELFTEST FAILED: truncate didn't resize file (expected 10, got {})", contents.len());
                return;
            }
            crate::serial_println!("[nextfs] truncate: OK (file now {} bytes)", contents.len());
        }
        Err(e) => {
            crate::serial_println!("[nextfs] SELFTEST FAILED: read after truncate error: {}", e);
            return;
        }
    }

    // 10. Test file deletion: create a temp file, then unlink it.
    let temp_file = match fs.create_file(test_dir, "temp.txt") {
        Ok(inode) => inode,
        Err(e) => {
            crate::serial_println!("[nextfs] SELFTEST FAILED: create temp file error: {}", e);
            return;
        }
    };
    if let Err(e) = fs.write_file(temp_file, b"temporary data") {
        crate::serial_println!("[nextfs] SELFTEST FAILED: write temp file error: {}", e);
        return;
    }
    
    // Verify temp file exists in directory.
    match fs.dir_list(test_dir) {
        Ok(entries) => {
            if entries.len() != 4 {
                crate::serial_println!("[nextfs] SELFTEST FAILED: /test should have 4 entries after temp creation, got {}", entries.len());
                return;
            }
        }
        Err(e) => {
            crate::serial_println!("[nextfs] SELFTEST FAILED: dir_list error: {}", e);
            return;
        }
    }
    
    // Now unlink the temp file.
    if let Err(e) = fs.unlink(test_dir, "temp.txt") {
        crate::serial_println!("[nextfs] SELFTEST FAILED: unlink error: {}", e);
        return;
    }
    
    // Verify temp file is gone.
    match fs.dir_list(test_dir) {
        Ok(entries) => {
            if entries.len() != 3 {
                crate::serial_println!("[nextfs] SELFTEST FAILED: /test should have 3 entries after unlink, got {}", entries.len());
                return;
            }
            crate::serial_println!("[nextfs] unlink: OK (file removed)");
        }
        Err(e) => {
            crate::serial_println!("[nextfs] SELFTEST FAILED: dir_list after unlink error: {}", e);
            return;
        }
    }

    // 11. Test directory removal: create a subdir, then rmdir it.
    let subdir = match fs.create_dir(test_dir, "subdir") {
        Ok(inode) => inode,
        Err(e) => {
            crate::serial_println!("[nextfs] SELFTEST FAILED: create subdir error: {}", e);
            return;
        }
    };
    
    if let Err(e) = fs.rmdir(test_dir, "subdir") {
        crate::serial_println!("[nextfs] SELFTEST FAILED: rmdir error: {}", e);
        return;
    }
    crate::serial_println!("[nextfs] rmdir: OK (empty directory removed)");

    // 11b. Permission / ownership enforcement.
    {
        use crate::fs::nextfs::{MAY_READ, MAY_WRITE};
        const ROOT: u16 = 0;
        const ALICE: u16 = 1000;

        let perm_file = match fs.create_file(test_dir, "perm.txt") {
            Ok(inode) => inode,
            Err(e) => {
                crate::serial_println!("[nextfs] SELFTEST FAILED: create perm.txt error: {}", e);
                return;
            }
        };

        // Owned by root, mode rw------- (0o600).
        if let Err(e) = fs.set_owner_mode(perm_file, ROOT, ROOT, 0o600) {
            crate::serial_println!("[nextfs] SELFTEST FAILED: set_owner_mode error: {}", e);
            return;
        }

        // Owner (root) may read+write.
        if fs.check_permission(perm_file, ROOT, ROOT, MAY_READ | MAY_WRITE).is_err() {
            crate::serial_println!("[nextfs] SELFTEST FAILED: owner denied on 0o600 file");
            return;
        }

        // A non-owner, non-root user must be denied on a 0o600 file.
        match fs.check_permission(perm_file, ALICE, ALICE, MAY_READ) {
            Err(crate::fs::FsError::PermissionDenied) => {}
            other => {
                crate::serial_println!(
                    "[nextfs] SELFTEST FAILED: expected PermissionDenied for uid 1000, got {:?}",
                    other
                );
                return;
            }
        }

        // After widening to 0o644, the same user may now read.
        if let Err(e) = fs.chmod(perm_file, 0o644) {
            crate::serial_println!("[nextfs] SELFTEST FAILED: chmod error: {}", e);
            return;
        }
        if fs.check_permission(perm_file, ALICE, ALICE, MAY_READ).is_err() {
            crate::serial_println!("[nextfs] SELFTEST FAILED: read denied after chmod 0o644");
            return;
        }
        // ...but writing is still owner-only.
        match fs.check_permission(perm_file, ALICE, ALICE, MAY_WRITE) {
            Err(crate::fs::FsError::PermissionDenied) => {}
            other => {
                crate::serial_println!(
                    "[nextfs] SELFTEST FAILED: expected write denied on 0o644 for uid 1000, got {:?}",
                    other
                );
                return;
            }
        }

        // chown the file to Alice; she becomes the owner and regains write.
        if let Err(e) = fs.chown(perm_file, ALICE, ALICE) {
            crate::serial_println!("[nextfs] SELFTEST FAILED: chown error: {}", e);
            return;
        }
        if fs.check_permission(perm_file, ALICE, ALICE, MAY_WRITE).is_err() {
            crate::serial_println!("[nextfs] SELFTEST FAILED: owner write denied after chown");
            return;
        }

        // Clean up the scratch file.
        let _ = fs.unlink(test_dir, "perm.txt");
        crate::serial_println!("[nextfs] permissions: OK (mode + owner enforcement verified)");
    }

    // 12. Sync to disk.
    if let Err(e) = fs.sync() {
        crate::serial_println!("[nextfs] SELFTEST FAILED: sync error: {}", e);
        return;
    }
    crate::serial_println!("[nextfs] sync: OK");

    // 13. Test syscall path: mount the filesystem globally, init FDs for task 0, then use syscalls.
    drop(fs); // Drop the local fs instance.
    if let Err(e) = crate::fs::mount(dev_idx) {
        crate::serial_println!("[nextfs] SELFTEST FAILED: global mount error: {:?}", e);
        return;
    }
    crate::fs::init_task_fds(0); // Task 0 for boot-time test.

    // Open a file via syscall.
    let fd = match crate::fs::open(0, "/test/hello.txt", crate::fs::OpenFlags::rdonly()) {
        Ok(fd) => fd,
        Err(e) => {
            crate::serial_println!("[nextfs] SELFTEST FAILED: syscall open error: {:?}", e);
            return;
        }
    };
    crate::serial_println!("[nextfs] syscall open: OK (fd={})", fd);
    
    // Read via syscall.
    let mut buf = [0u8; 20];
    match crate::fs::read(0, fd, &mut buf) {
        Ok(n) => {
            if n != 10 {
                crate::serial_println!("[nextfs] SELFTEST FAILED: syscall read returned {} bytes, expected 10", n);
                return;
            }
            crate::serial_println!("[nextfs] syscall read: OK ({} bytes)", n);
        }
        Err(e) => {
            crate::serial_println!("[nextfs] SELFTEST FAILED: syscall read error: {:?}", e);
            return;
        }
    }
    
    // Close via syscall.
    if let Err(e) = crate::fs::close(0, fd) {
        crate::serial_println!("[nextfs] SELFTEST FAILED: syscall close error: {:?}", e);
        return;
    }
    crate::serial_println!("[nextfs] syscall close: OK");

    // 14. stdin path: inject characters into the console buffer and read them
    //     back through fd 0 (STDIN) of task 0, exactly as a user program would.
    {
        for c in "Pi".chars() {
            console::inject_char(c);
        }
        let mut sbuf = [0u8; 8];
        match crate::fs::read(0, crate::fs::STDIN, &mut sbuf) {
            Ok(n) if &sbuf[..n] == b"Pi" => {
                crate::serial_println!("[nextfs] stdin read: OK ({} bytes: {:?})", n, "Pi");
            }
            Ok(n) => {
                crate::serial_println!(
                    "[nextfs] SELFTEST FAILED: stdin read returned {:?}",
                    core::str::from_utf8(&sbuf[..n]).unwrap_or("<bad utf8>")
                );
                return;
            }
            Err(e) => {
                crate::serial_println!("[nextfs] SELFTEST FAILED: stdin read error: {:?}", e);
                return;
            }
        }
    }

    crate::serial_println!("[nextfs] === SELFTEST PASSED ===");
}
