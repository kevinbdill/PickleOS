//! System-call interface — the kernel/user boundary.
//!
//! User-space (and, for demonstration, kernel tasks) request kernel services by
//! issuing `int 0x80` with the syscall number in `rax` and arguments in
//! `rdi, rsi, rdx` (a Linux-like register ABI). The return value comes back in
//! `rax`.
//!
//! ## How a syscall flows through the kernel
//! 1. `int 0x80` traps into [`syscall_stub`] (assembly, below). The IDT entry
//!    for vector `0x80` has DPL=3 so ring-3 code is allowed to invoke it.
//! 2. [`syscall_stub`] saves every general-purpose register into a
//!    [`SyscallFrame`] on the stack and passes a pointer to it to
//!    [`syscall_dispatch`].
//! 3. [`syscall_dispatch`] decodes the number and arguments, performs the
//!    requested service (often delegating to [`crate::ipc`], [`crate::task`] or
//!    [`crate::capability`]), and returns a `u64`.
//! 4. The stub stores that return value back into the frame's `rax` slot,
//!    restores all registers, and `iretq`s back to the caller.
//!
//! Because services like drivers and file systems live in user space, *almost
//! every* real operation a program performs ultimately becomes an IPC call;
//! these syscalls are the small, trusted primitives that make that possible.

use crate::task;
use crate::{print, serial_print, serial_println};
use core::arch::asm;

// --- Syscall numbers -------------------------------------------------------
pub const SYS_PRINT: u64 = 1; // (ptr, len) -> bytes written
pub const SYS_GETPID: u64 = 2; // () -> current task id
pub const SYS_EXIT: u64 = 3; // () -> never returns
pub const SYS_YIELD: u64 = 4; // () -> 0   (voluntarily reschedule)
pub const SYS_TICKS: u64 = 5; // () -> timer ticks since boot
pub const SYS_IPC_SEND: u64 = 6; // (ep, tag) -> 0
pub const SYS_IPC_RECV: u64 = 7; // (ep) -> tag of received message
pub const SYS_CAP_CHECK: u64 = 8; // (slot, rights) -> 1 if held else 0
pub const SYS_SLEEP: u64 = 9; // (ticks) -> 0 (after wake)
pub const SYS_SPAWN: u64 = 10; // (binary_ptr, binary_len, name_ptr) -> task_id
pub const SYS_MMAP: u64 = 11; // (addr, len, prot) -> mapped_addr
pub const SYS_MUNMAP: u64 = 12; // (addr, len) -> 0 or error
pub const SYS_OPEN: u64 = 13; // (path_ptr, path_len, flags) -> fd or error
pub const SYS_READ: u64 = 14; // (fd, buf_ptr, count) -> bytes_read or error
pub const SYS_WRITE: u64 = 15; // (fd, buf_ptr, count) -> bytes_written or error
pub const SYS_CLOSE: u64 = 16; // (fd) -> 0 or error
pub const SYS_LSEEK: u64 = 17; // (fd, offset, whence) -> new_pos or error
pub const SYS_UNLINK: u64 = 18; // (path_ptr, path_len) -> 0 or error
pub const SYS_RMDIR: u64 = 19; // (path_ptr, path_len) -> 0 or error
pub const SYS_MKDIR: u64 = 20; // (path_ptr, path_len) -> 0 or error
pub const SYS_TRUNCATE: u64 = 21; // (path_ptr, path_len, size) -> 0 or error
pub const SYS_CHMOD: u64 = 22; // (path_ptr, path_len, mode) -> 0 or error
pub const SYS_CHOWN: u64 = 23; // (path_ptr, path_len, uid<<16|gid) -> 0 or error
pub const SYS_STAT: u64 = 24; // (path_ptr, path_len, statbuf_ptr) -> 0 or error
// --- Process management ----------------------------------------------------
pub const SYS_EXIT2: u64 = 25; // (status) -> never returns (proper exit + status)
pub const SYS_WAIT: u64 = 26; // (status_ptr) -> child pid (or -1 if no children)
pub const SYS_EXEC: u64 = 27; // (path_ptr, path_len, argv, envp) -> -1 on failure; else no return
pub const SYS_FORK: u64 = 28; // () -> child pid in parent, 0 in child, -1 on error
pub const SYS_PIPE: u64 = 29; // (fds_ptr) -> 0; writes [read_fd, write_fd] as two u32
// --- Identity & signals ----------------------------------------------------
pub const SYS_GETPPID: u64 = 30; // () -> parent process id (0 if none)
pub const SYS_KILL: u64 = 31; // (pid, sig) -> 0 or -1
pub const SYS_SIGNAL: u64 = 32; // (sig, handler, restorer) -> previous handler or -1
pub const SYS_SIGRETURN: u64 = 33; // () -> resume interrupted context (internal, via trampoline)
// --- Window server (display server foundation) -----------------------------
pub const SYS_WIN_CREATE: u64 = 34; // (w, h, title_ptr, title_len[r10]) -> win_id or -1
pub const SYS_WIN_COMMIT: u64 = 35; // (win_id, buf_ptr, byte_len) -> 0 or -1
pub const SYS_WIN_POLL: u64 = 36; // (win_id, event_ptr[16]) -> 1 got / 0 none / -1 err
pub const SYS_WIN_DESTROY: u64 = 37; // (win_id) -> 0 or -1
pub const SYS_WIN_INFO: u64 = 38; // (win_id, info_ptr[16]) -> 0 or -1
// --- Networking ------------------------------------------------------------
pub const SYS_SOCKET: u64 = 39; // (domain, type, protocol) -> fd or -1
pub const SYS_BIND: u64 = 40; // (sockfd, addr_ptr, addrlen) -> 0 or -1
pub const SYS_CONNECT: u64 = 41; // (sockfd, addr_ptr, addrlen) -> 0 or -1
pub const SYS_LISTEN: u64 = 42; // (sockfd, backlog) -> 0 or -1
pub const SYS_ACCEPT: u64 = 43; // (sockfd, addr_ptr, addrlen_ptr) -> new_fd or -1
pub const SYS_SEND: u64 = 44; // (sockfd, buf_ptr, len, flags) -> bytes_sent or -1
pub const SYS_RECV: u64 = 45; // (sockfd, buf_ptr, len, flags) -> bytes_recv or -1
pub const SYS_SHUTDOWN: u64 = 46; // (sockfd, how) -> 0 or -1

/// Snapshot of the caller's general-purpose registers, built by [`syscall_stub`].
/// Field order MUST match the push order in the assembly stub (last pushed =
/// lowest address = first field).
#[repr(C)]
pub struct SyscallFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub r11: u64,
    pub rcx: u64,
    pub r9: u64,
    pub r8: u64,
    pub r10: u64,
    pub rdx: u64, // arg2
    pub rsi: u64, // arg1
    pub rdi: u64, // arg0
    pub rax: u64, // syscall number in / return value out
}

// The low-level entry point for `int 0x80`. Written in assembly so we control
// exactly how registers are saved/restored around the dispatcher call.
core::arch::global_asm!(
    r#"
.global syscall_stub
syscall_stub:
    push rax
    push rdi
    push rsi
    push rdx
    push r10
    push r8
    push r9
    push rcx
    push r11
    push rbx
    push rbp
    push r12
    push r13
    push r14
    push r15
    mov rdi, rsp            # &SyscallFrame
    call syscall_dispatch
    mov [rsp + 14*8], rax   # store return value into the frame's rax slot
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbp
    pop rbx
    pop r11
    pop rcx
    pop r9
    pop r8
    pop r10
    pop rdx
    pop rsi
    pop rdi
    pop rax
    iretq
"#
);

extern "C" {
    /// The assembly entry registered in the IDT for vector `0x80`.
    pub fn syscall_stub();
}

/// Check if a pointer is in user space (below the kernel boundary).
/// This is a simple check; a full implementation would verify the pages are mapped.
fn is_user_pointer(ptr: u64, len: usize) -> bool {
    // User space is below 0x0000_8000_0000_0000 (the lower half).
    const USER_SPACE_MAX: u64 = 0x0000_8000_0000_0000;
    if ptr >= USER_SPACE_MAX {
        return false;
    }
    // Check for overflow
    if let Some(end) = ptr.checked_add(len as u64) {
        end <= USER_SPACE_MAX
    } else {
        false
    }
}

/// Extract socket ID from a file descriptor. Returns None if FD is not a socket.
fn get_socket_id(task_id: u32, fd: u32) -> Option<u32> {
    use crate::fs::vfs::{FD_TABLES, FileType};
    
    let tables = FD_TABLES.lock();
    let table = tables.get(&task_id)?;
    let file = table.files.get(&fd)?;
    
    if let FileType::Socket(socket_id) = file.file_type {
        Some(socket_id)
    } else {
        None
    }
}

/// Copy a NULL-terminated array of NUL-terminated C strings (a `char **`, as
/// passed to `execve`) out of the caller's address space into owned `String`s.
///
/// `base` points at the array of pointers; a null `base` (or a null first
/// entry) yields an empty vector. When `validate` is set (ring-3 callers) every
/// pointer dereferenced is bounds-checked with [`is_user_pointer`]. To bound the
/// work done on behalf of an untrusted caller we cap the number of entries and
/// the length of each string.
fn copy_user_cstr_array(base: u64, validate: bool) -> alloc::vec::Vec<alloc::string::String> {
    const MAX_ENTRIES: usize = 64;
    const MAX_STR_LEN: usize = 256;
    let mut out: alloc::vec::Vec<alloc::string::String> = alloc::vec::Vec::new();
    if base == 0 {
        return out;
    }
    if validate && !is_user_pointer(base, 8) {
        return out;
    }
    let ptr_array = base as *const u64;
    for i in 0..MAX_ENTRIES {
        // Read the i-th pointer in the array.
        if validate && !is_user_pointer(base + (i as u64) * 8, 8) {
            break;
        }
        let str_ptr = unsafe { core::ptr::read(ptr_array.add(i)) };
        if str_ptr == 0 {
            break; // NULL terminator
        }
        if validate && !is_user_pointer(str_ptr, 1) {
            break;
        }
        // Walk the C string up to MAX_STR_LEN looking for the NUL.
        let mut len = 0usize;
        while len < MAX_STR_LEN {
            if validate && !is_user_pointer(str_ptr + len as u64, 1) {
                break;
            }
            let b = unsafe { core::ptr::read((str_ptr as *const u8).add(len)) };
            if b == 0 {
                break;
            }
            len += 1;
        }
        let bytes = unsafe { core::slice::from_raw_parts(str_ptr as *const u8, len) };
        match core::str::from_utf8(bytes) {
            Ok(s) => out.push(alloc::string::String::from(s)),
            Err(_) => out.push(alloc::string::String::new()),
        }
    }
    out
}

/// Implementation of anonymous memory mapping for user processes.
/// This is a simplified version that allocates and maps pages in the user address space.
fn sys_mmap_impl(addr: u64, len: usize, _prot: u32) -> Result<usize, ()> {
    use crate::memory;

    if len == 0 {
        return Err(());
    }

    // Round up to whole pages.
    let page_count = (len + 4095) / 4096;

    // The mapping must land in the *calling task's* private address space, not
    // the kernel's. Grab its CR3 and bump its per-task mmap pointer so repeated
    // mmaps (and concurrent mmaps from other tasks) never collide. We ignore
    // the user-supplied `addr` hint for now and hand out addresses from the
    // task's heap window.
    let _ = addr;
    let (user_cr3, start_addr) = task::scheduler::with(|s| {
        let t = &mut s.tasks[s.current];
        let cr3 = t.user_cr3;
        let start = t.mmap_next;
        if cr3.is_some() {
            t.mmap_next = start + (page_count as u64) * 4096;
        }
        (cr3, start)
    });

    let user_cr3 = user_cr3.ok_or(())?;

    // Map the region into that task's page tables.
    memory::with_memory(|mem| {
        mem.map_user_region(user_cr3, start_addr, page_count, true)
    })?;

    Ok(start_addr as usize)
}

/// Implementation of memory unmapping for user processes.
fn sys_munmap_impl(addr: u64, len: usize) -> Result<(), ()> {
    use x86_64::structures::paging::{Mapper, Page, Size4KiB};
    use x86_64::VirtAddr;
    
    if len == 0 {
        return Ok(());
    }
    
    // Round up to page size
    let page_count = (len + 4095) / 4096;
    
    // Get the current task's CR3 to work with the right page tables
    let user_cr3 = task::scheduler::with(|s| {
        s.tasks[s.current].user_cr3
    });
    
    if user_cr3.is_none() {
        return Err(());
    }
    
    // Unmap pages
    crate::memory::with_memory(|mem| {
        let start_page = Page::<Size4KiB>::containing_address(VirtAddr::new(addr));
        
        for i in 0..page_count {
            let page = start_page + i as u64;
            let (_frame, flush) = mem.mapper.unmap(page).map_err(|_| ())?;
            flush.flush();
        }
        
        Ok(())
    })
}

/// The Rust syscall dispatcher. Decodes [`SyscallFrame`], performs the request,
/// then delivers any pending signal before returning the value to place in
/// `rax`. Signal delivery happens here (the kernel/user boundary) so a caught
/// signal is serviced the next time the target task returns from a syscall.
#[no_mangle]
extern "C" fn syscall_dispatch(frame: *mut SyscallFrame) -> u64 {
    let f = unsafe { &mut *frame };
    let num = f.rax;
    let ret = syscall_dispatch_inner(f);
    // SYS_SIGRETURN already restored the interrupted context; do not re-arm a
    // handler on top of the value it just returned (it returns through the
    // normal path so the restored rax is preserved).
    if num != SYS_SIGRETURN {
        task::deliver_pending_signals(f, ret);
    }
    ret
}

/// The core syscall decode + service routine. Returns the value to place in
/// `rax` (signal delivery is layered on top by [`syscall_dispatch`]).
fn syscall_dispatch_inner(f: &mut SyscallFrame) -> u64 {
    let (num, a0, a1, a2) = (f.rax, f.rdi, f.rsi, f.rdx);
    let _ = a2; // a2 is reserved for future 3-argument syscalls.

    match num {
        SYS_PRINT => {
            // a0 = pointer to UTF-8 bytes, a1 = length.
            // Validate user pointer if this is a ring 3 syscall.
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User && !is_user_pointer(a0, a1 as usize) {
                serial_println!("[syscall] SYS_PRINT: invalid user pointer {:#x}", a0);
                return u64::MAX; // error
            }
            
            let slice = unsafe { core::slice::from_raw_parts(a0 as *const u8, a1 as usize) };
            if let Ok(s) = core::str::from_utf8(slice) {
                // Mirror to both the VGA console and the serial port. Headless
                // QEMU only surfaces serial output, so user-task prints were
                // previously invisible during testing.
                print!("{}", s);
                serial_print!("{}", s);
            }
            a1 // bytes "written"
        }
        SYS_GETPID => task::current_id(),
        SYS_EXIT | SYS_EXIT2 => {
            // Terminate the current process with an exit status (a0). Closes
            // fds, reparents children to init, wakes a waiting parent, and
            // reschedules. Never returns. SYS_EXIT (3) is kept as an alias of
            // SYS_EXIT2 (25) for backward compatibility with existing binaries.
            task::do_exit(a0 as i32);
        }
        SYS_YIELD => {
            task::yield_now();
            0
        }
        SYS_TICKS => task::scheduler::ticks(),
        SYS_IPC_SEND => {
            crate::ipc::send(a0, crate::ipc::Message::new(a1));
            0
        }
        SYS_IPC_RECV => {
            let msg = crate::ipc::receive(a0);
            msg.tag
        }
        SYS_CAP_CHECK => {
            let id = task::current_id();
            let held = crate::capability::check(id, a0 as usize, crate::capability::Rights(a1 as u32));
            held as u64
        }
        SYS_SLEEP => {
            // a0 = number of ticks to sleep
            task::sleep_ticks(a0);
            0
        }
        SYS_SPAWN => {
            // a0 = pointer to ELF binary, a1 = length, a2 = pointer to name string
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User {
                if !is_user_pointer(a0, a1 as usize) {
                    serial_println!("[syscall] SYS_SPAWN: invalid binary pointer {:#x}", a0);
                    return u64::MAX;
                }
                if a2 != 0 && !is_user_pointer(a2, 256) {
                    serial_println!("[syscall] SYS_SPAWN: invalid name pointer {:#x}", a2);
                    return u64::MAX;
                }
            }
            
            // For spawn from user space, we need to copy the binary data since it needs 'static lifetime
            // For now, we'll only allow spawning from kernel space or use embedded binaries
            // This is a limitation we'll address in Phase 2 by having binaries in VFS
            serial_println!("[syscall] SYS_SPAWN: not yet implemented for dynamic loading");
            u64::MAX // not implemented yet
        }
        SYS_MMAP => {
            // a0 = suggested address (0 = kernel chooses), a1 = length, a2 = protection flags
            // For now, we'll implement a simple anonymous memory mapping
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring != task::Ring::User {
                // Only user tasks can mmap for now
                return u64::MAX;
            }
            
            // Simple implementation: allocate pages in user space
            match sys_mmap_impl(a0, a1 as usize, a2 as u32) {
                Ok(addr) => addr as u64,
                Err(_) => u64::MAX,
            }
        }
        SYS_MUNMAP => {
            // a0 = address, a1 = length
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring != task::Ring::User {
                return u64::MAX;
            }
            
            match sys_munmap_impl(a0, a1 as usize) {
                Ok(_) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_OPEN => {
            // a0 = path_ptr, a1 = path_len, a2 = flags
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User && !is_user_pointer(a0, a1 as usize) {
                serial_println!("[syscall] SYS_OPEN: invalid user pointer {:#x}", a0);
                return u64::MAX;
            }

            let path_slice = unsafe { core::slice::from_raw_parts(a0 as *const u8, a1 as usize) };
            let path = match core::str::from_utf8(path_slice) {
                Ok(s) => s,
                Err(_) => return u64::MAX,
            };

            let flags = sys_open_flags_from_raw(a2);
            let task_id = task::current_id() as u32;
            
            match crate::fs::open(task_id, path, flags) {
                Ok(fd) => fd as u64,
                Err(_) => u64::MAX,
            }
        }
        SYS_READ => {
            // a0 = fd, a1 = buf_ptr, a2 = count
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User && !is_user_pointer(a1, a2 as usize) {
                serial_println!("[syscall] SYS_READ: invalid user pointer {:#x}", a1);
                return u64::MAX;
            }

            let buf = unsafe { core::slice::from_raw_parts_mut(a1 as *mut u8, a2 as usize) };
            let task_id = task::current_id() as u32;
            
            match crate::fs::read(task_id, a0 as u32, buf) {
                Ok(n) => n as u64,
                Err(_) => u64::MAX,
            }
        }
        SYS_WRITE => {
            // a0 = fd, a1 = buf_ptr, a2 = count
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User && !is_user_pointer(a1, a2 as usize) {
                serial_println!("[syscall] SYS_WRITE: invalid user pointer {:#x}", a1);
                return u64::MAX;
            }

            let buf = unsafe { core::slice::from_raw_parts(a1 as *const u8, a2 as usize) };
            let task_id = task::current_id() as u32;
            
            match crate::fs::write(task_id, a0 as u32, buf) {
                Ok(n) => n as u64,
                Err(_) => u64::MAX,
            }
        }
        SYS_CLOSE => {
            // a0 = fd
            let task_id = task::current_id() as u32;
            
            match crate::fs::close(task_id, a0 as u32) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_LSEEK => {
            // a0 = fd, a1 = offset (i64 as u64), a2 = whence
            let task_id = task::current_id() as u32;
            let offset = a1 as i64;
            let whence = match a2 {
                0 => crate::fs::SeekWhence::Set,
                1 => crate::fs::SeekWhence::Current,
                2 => crate::fs::SeekWhence::End,
                _ => return u64::MAX,
            };
            
            match crate::fs::seek(task_id, a0 as u32, offset, whence) {
                Ok(pos) => pos,
                Err(_) => u64::MAX,
            }
        }
        SYS_UNLINK => {
            // a0 = path_ptr, a1 = path_len
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User && !is_user_pointer(a0, a1 as usize) {
                serial_println!("[syscall] SYS_UNLINK: invalid user pointer {:#x}", a0);
                return u64::MAX;
            }

            let path_slice = unsafe { core::slice::from_raw_parts(a0 as *const u8, a1 as usize) };
            let path = match core::str::from_utf8(path_slice) {
                Ok(s) => s,
                Err(_) => return u64::MAX,
            };

            let task_id = task::current_id() as u32;
            
            match crate::fs::unlink(task_id, path) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_RMDIR => {
            // a0 = path_ptr, a1 = path_len
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User && !is_user_pointer(a0, a1 as usize) {
                serial_println!("[syscall] SYS_RMDIR: invalid user pointer {:#x}", a0);
                return u64::MAX;
            }

            let path_slice = unsafe { core::slice::from_raw_parts(a0 as *const u8, a1 as usize) };
            let path = match core::str::from_utf8(path_slice) {
                Ok(s) => s,
                Err(_) => return u64::MAX,
            };

            let task_id = task::current_id() as u32;
            
            match crate::fs::rmdir(task_id, path) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_MKDIR => {
            // a0 = path_ptr, a1 = path_len
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User && !is_user_pointer(a0, a1 as usize) {
                serial_println!("[syscall] SYS_MKDIR: invalid user pointer {:#x}", a0);
                return u64::MAX;
            }

            let path_slice = unsafe { core::slice::from_raw_parts(a0 as *const u8, a1 as usize) };
            let path = match core::str::from_utf8(path_slice) {
                Ok(s) => s,
                Err(_) => return u64::MAX,
            };

            let task_id = task::current_id() as u32;
            
            match crate::fs::mkdir(task_id, path) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_TRUNCATE => {
            // a0 = path_ptr, a1 = path_len, a2 = size
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User && !is_user_pointer(a0, a1 as usize) {
                serial_println!("[syscall] SYS_TRUNCATE: invalid user pointer {:#x}", a0);
                return u64::MAX;
            }

            let path_slice = unsafe { core::slice::from_raw_parts(a0 as *const u8, a1 as usize) };
            let path = match core::str::from_utf8(path_slice) {
                Ok(s) => s,
                Err(_) => return u64::MAX,
            };

            let task_id = task::current_id() as u32;
            
            match crate::fs::truncate(task_id, path, a2) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_CHMOD => {
            // a0 = path_ptr, a1 = path_len, a2 = mode
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User && !is_user_pointer(a0, a1 as usize) {
                serial_println!("[syscall] SYS_CHMOD: invalid user pointer {:#x}", a0);
                return u64::MAX;
            }
            let path_slice = unsafe { core::slice::from_raw_parts(a0 as *const u8, a1 as usize) };
            let path = match core::str::from_utf8(path_slice) {
                Ok(s) => s,
                Err(_) => return u64::MAX,
            };
            let task_id = task::current_id() as u32;
            match crate::fs::chmod(task_id, path, a2 as u16) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_CHOWN => {
            // a0 = path_ptr, a1 = path_len, a2 = (uid << 16) | gid
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User && !is_user_pointer(a0, a1 as usize) {
                serial_println!("[syscall] SYS_CHOWN: invalid user pointer {:#x}", a0);
                return u64::MAX;
            }
            let path_slice = unsafe { core::slice::from_raw_parts(a0 as *const u8, a1 as usize) };
            let path = match core::str::from_utf8(path_slice) {
                Ok(s) => s,
                Err(_) => return u64::MAX,
            };
            let uid = ((a2 >> 16) & 0xFFFF) as u16;
            let gid = (a2 & 0xFFFF) as u16;
            let task_id = task::current_id() as u32;
            match crate::fs::chown(task_id, path, uid, gid) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_STAT => {
            // a0 = path_ptr, a1 = path_len, a2 = statbuf_ptr (out, 24 bytes).
            // Layout: u8 type, u16 mode, u16 uid, u16 gid, u64 size, u32 mtime.
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User
                && (!is_user_pointer(a0, a1 as usize) || !is_user_pointer(a2, 24))
            {
                serial_println!("[syscall] SYS_STAT: invalid user pointer");
                return u64::MAX;
            }
            let path_slice = unsafe { core::slice::from_raw_parts(a0 as *const u8, a1 as usize) };
            let path = match core::str::from_utf8(path_slice) {
                Ok(s) => s,
                Err(_) => return u64::MAX,
            };
            let task_id = task::current_id() as u32;
            match crate::fs::stat(task_id, path) {
                Ok(st) => {
                    // Serialize into the user-provided buffer.
                    let out = unsafe { core::slice::from_raw_parts_mut(a2 as *mut u8, 24) };
                    out[0] = st.inode_type;
                    out[1] = 0; // pad
                    out[2..4].copy_from_slice(&st.mode.to_le_bytes());
                    out[4..6].copy_from_slice(&st.uid.to_le_bytes());
                    out[6..8].copy_from_slice(&st.gid.to_le_bytes());
                    out[8..16].copy_from_slice(&st.size.to_le_bytes());
                    out[16..20].copy_from_slice(&st.mtime.to_le_bytes());
                    0
                }
                Err(_) => u64::MAX,
            }
        }
        SYS_WAIT => {
            // a0 = pointer to an i32 where the child's exit status is written
            // (may be null to ignore). Blocks until any child terminates, reaps
            // the zombie, and returns its PID. Returns u64::MAX if no children.
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if a0 != 0
                && current_ring == task::Ring::User
                && !is_user_pointer(a0, 4)
            {
                serial_println!("[syscall] SYS_WAIT: invalid user status pointer {:#x}", a0);
                return u64::MAX;
            }
            task::do_wait(a0)
        }
        SYS_EXEC => {
            // a0 = path_ptr, a1 = path_len, a2 = argv (char**), a3(r10) = envp
            // (char**). argv/envp are NULL-terminated arrays of NUL-terminated
            // C strings (either may be null). Replaces the current process image
            // with the ELF at `path`, preserving PID/parent/fds. On success it
            // does not return to the caller (frame is rewritten to the new entry
            // point); on failure it returns u64::MAX.
            let a3 = f.r10;
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User && !is_user_pointer(a0, a1 as usize) {
                serial_println!("[syscall] SYS_EXEC: invalid user path pointer {:#x}", a0);
                return u64::MAX;
            }
            let path_slice = unsafe { core::slice::from_raw_parts(a0 as *const u8, a1 as usize) };
            let path = match core::str::from_utf8(path_slice) {
                Ok(s) => alloc::string::String::from(s),
                Err(_) => {
                    serial_println!("[syscall] SYS_EXEC: path is not valid UTF-8");
                    return u64::MAX;
                }
            };
            // Copy argv/envp out of the (soon-to-be-destroyed) user address
            // space before the loader replaces it.
            let is_user = current_ring == task::Ring::User;
            let argv = copy_user_cstr_array(a2, is_user);
            let envp = copy_user_cstr_array(a3, is_user);
            task::do_exec(f, &path, &argv, &envp)
        }
        SYS_FORK => {
            // Duplicates the current process: copies the address space and fd
            // table, assigns a new PID to the child, and registers it as a
            // child of the parent. Returns the child's PID in the parent and 0
            // in the child. Returns u64::MAX on error.
            task::do_fork(f)
        }
        SYS_PIPE => {
            // a0 = pointer to an array of two u32 the kernel fills with
            // [read_fd, write_fd]. Returns 0 on success, u64::MAX on error.
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User && !is_user_pointer(a0, 8) {
                serial_println!("[syscall] SYS_PIPE: invalid user pointer {:#x}", a0);
                return u64::MAX;
            }
            if a0 == 0 {
                return u64::MAX;
            }
            let task_id = task::current_id() as u32;
            match crate::fs::pipe(task_id) {
                Ok((read_fd, write_fd)) => {
                    let out = unsafe { core::slice::from_raw_parts_mut(a0 as *mut u32, 2) };
                    out[0] = read_fd;
                    out[1] = write_fd;
                    0
                }
                Err(_) => u64::MAX,
            }
        }
        SYS_GETPPID => task::current_ppid(),
        SYS_KILL => {
            // a0 = target pid, a1 = signal number. Returns 0 on success,
            // u64::MAX if the target does not exist or the signal is invalid.
            task::do_kill(a0, a1 as u32)
        }
        SYS_SIGNAL => {
            // a0 = signal number, a1 = handler (user fn ptr; 0=SIG_DFL, 1=SIG_IGN),
            // a2 = restorer trampoline (user code that issues SYS_SIGRETURN).
            // Returns the previous handler, or u64::MAX on error.
            task::do_signal(a0 as u32, a1, a2)
        }
        SYS_SIGRETURN => {
            // Restore the context saved when a signal handler was entered. The
            // frame is rewritten in place so the stub `iretq`s back to where the
            // task was interrupted; the returned value becomes the restored rax.
            task::do_sigreturn(f)
        }
        SYS_WIN_CREATE => {
            // a0 = width, a1 = height, a2 = title_ptr, r10 = title_len.
            // Creates a window owned by the calling task and returns its id.
            let title_len = f.r10 as usize;
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User && a2 != 0 && !is_user_pointer(a2, title_len) {
                serial_println!("[syscall] SYS_WIN_CREATE: invalid title pointer {:#x}", a2);
                return u64::MAX;
            }
            let title = if a2 != 0 && title_len > 0 {
                let slice = unsafe { core::slice::from_raw_parts(a2 as *const u8, title_len) };
                core::str::from_utf8(slice).unwrap_or("window")
            } else {
                "window"
            };
            let owner = task::current_id();
            match crate::wm::create_window(owner, a0 as usize, a1 as usize, title) {
                Some(id) => id,
                None => u64::MAX,
            }
        }
        SYS_WIN_COMMIT => {
            // a0 = win_id, a1 = buf_ptr (array of u32 0x00RRGGBB), a2 = byte_len.
            let win_id = a0;
            let byte_len = a2 as usize;
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User {
                if !crate::wm::is_owner(win_id, task::current_id()) {
                    return u64::MAX;
                }
                if !is_user_pointer(a1, byte_len) {
                    serial_println!("[syscall] SYS_WIN_COMMIT: invalid buffer pointer {:#x}", a1);
                    return u64::MAX;
                }
            }
            // Reinterpret the byte buffer as u32 pixels (truncating any partial
            // trailing pixel).
            let px_count = byte_len / 4;
            let pixels = unsafe { core::slice::from_raw_parts(a1 as *const u32, px_count) };
            if crate::wm::commit(win_id, pixels) {
                0
            } else {
                u64::MAX
            }
        }
        SYS_WIN_POLL => {
            // a0 = win_id, a1 = pointer to a 16-byte WmEvent buffer.
            let win_id = a0;
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User {
                if !crate::wm::is_owner(win_id, task::current_id()) {
                    return u64::MAX;
                }
                if !is_user_pointer(a1, 16) {
                    serial_println!("[syscall] SYS_WIN_POLL: invalid event pointer {:#x}", a1);
                    return u64::MAX;
                }
            }
            match crate::wm::poll_event(win_id) {
                Some(ev) => {
                    let out = unsafe { core::slice::from_raw_parts_mut(a1 as *mut u8, 16) };
                    out.copy_from_slice(&ev.to_bytes());
                    1
                }
                None => 0,
            }
        }
        SYS_WIN_DESTROY => {
            // a0 = win_id.
            let win_id = a0;
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User && !crate::wm::is_owner(win_id, task::current_id()) {
                return u64::MAX;
            }
            if crate::wm::destroy_window(win_id) {
                0
            } else {
                u64::MAX
            }
        }
        SYS_WIN_INFO => {
            // a0 = win_id, a1 = pointer to 16 bytes: u32 w, u32 h, i32 x, i32 y.
            let win_id = a0;
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User {
                if !crate::wm::is_owner(win_id, task::current_id()) {
                    return u64::MAX;
                }
                if !is_user_pointer(a1, 16) {
                    return u64::MAX;
                }
            }
            match crate::wm::window_info(win_id) {
                Some((x, y, w, h)) => {
                    let out = unsafe { core::slice::from_raw_parts_mut(a1 as *mut u8, 16) };
                    out[0..4].copy_from_slice(&(w as u32).to_le_bytes());
                    out[4..8].copy_from_slice(&(h as u32).to_le_bytes());
                    out[8..12].copy_from_slice(&(x as i32).to_le_bytes());
                    out[12..16].copy_from_slice(&(y as i32).to_le_bytes());
                    0
                }
                None => u64::MAX,
            }
        }
        SYS_SOCKET => {
            // a0 = domain (AF_INET=2), a1 = type (SOCK_STREAM=1), a2 = protocol (0=auto).
            // Returns fd or -1 on error.
            const AF_INET: u64 = 2;
            const SOCK_STREAM: u64 = 1;
            
            if a0 != AF_INET || a1 != SOCK_STREAM {
                serial_println!("[syscall] SYS_SOCKET: unsupported domain/type");
                return u64::MAX;
            }
            
            match crate::net::stack::create_tcp_socket() {
                Ok(socket_id) => {
                    let task_id = task::current_id() as u32;
                    match crate::fs::socket(task_id, socket_id) {
                        Ok(fd) => fd as u64,
                        Err(_) => {
                            crate::net::stack::tcp_close(socket_id);
                            u64::MAX
                        }
                    }
                }
                Err(e) => {
                    serial_println!("[syscall] SYS_SOCKET: failed to create socket: {}", e);
                    u64::MAX
                }
            }
        }
        SYS_BIND => {
            // a0 = sockfd, a1 = addr_ptr (struct sockaddr_in), a2 = addrlen.
            // Returns 0 on success, -1 on error.
            let sockfd = a0 as u32;
            let task_id = task::current_id() as u32;
            
            // Validate user pointer
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User && !is_user_pointer(a1, a2 as usize) {
                return u64::MAX;
            }
            
            // Parse sockaddr_in: 2 bytes family, 2 bytes port (network order), 4 bytes addr
            if a2 < 8 {
                return u64::MAX;
            }
            
            let addr_bytes = unsafe { core::slice::from_raw_parts(a1 as *const u8, a2 as usize) };
            let port = u16::from_be_bytes([addr_bytes[2], addr_bytes[3]]);
            
            // Get socket ID from FD
            match get_socket_id(task_id, sockfd) {
                Some(socket_id) => {
                    match crate::net::stack::tcp_bind(socket_id, port) {
                        Ok(_) => 0,
                        Err(e) => {
                            serial_println!("[syscall] SYS_BIND: failed: {}", e);
                            u64::MAX
                        }
                    }
                }
                None => u64::MAX,
            }
        }
        SYS_LISTEN => {
            // a0 = sockfd, a1 = backlog (ignored for now).
            // Returns 0 on success, -1 on error.
            let sockfd = a0 as u32;
            let task_id = task::current_id() as u32;
            
            match get_socket_id(task_id, sockfd) {
                Some(socket_id) => {
                    // Use the port recorded by a prior SYS_BIND. If the socket was
                    // never bound, fall back to the standard echo port (7).
                    let port = crate::net::stack::bound_port(socket_id).unwrap_or(7);
                    match crate::net::stack::tcp_listen(socket_id, port) {
                        Ok(_) => 0,
                        Err(e) => {
                            serial_println!("[syscall] SYS_LISTEN: failed: {}", e);
                            u64::MAX
                        }
                    }
                }
                None => u64::MAX,
            }
        }
        SYS_ACCEPT => {
            // a0 = sockfd (a listening socket), a1 = addr_ptr (out, sockaddr_in,
            // may be 0), a2 = addrlen (size of addr buffer).
            //
            // smoltcp reuses the listening socket as the connection, so "accept"
            // blocks until the socket has an established peer, then returns the
            // same fd. The peer address is written to `addr_ptr` if provided.
            let sockfd = a0 as u32;
            let task_id = task::current_id() as u32;

            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            let want_addr = a1 != 0;
            if want_addr {
                if a2 < 8 {
                    return u64::MAX;
                }
                if current_ring == task::Ring::User && !is_user_pointer(a1, a2 as usize) {
                    return u64::MAX;
                }
            }

            let socket_id = match get_socket_id(task_id, sockfd) {
                Some(id) => id,
                None => return u64::MAX,
            };

            // Block (cooperatively) until a connection is established. The
            // network demo task drives stack polling, so we just yield.
            let mut spins: u64 = 0;
            const MAX_SPINS: u64 = 50_000_000;
            while !crate::net::stack::tcp_accept_ready(socket_id) {
                task::yield_now();
                spins += 1;
                if spins >= MAX_SPINS {
                    serial_println!("[syscall] SYS_ACCEPT: timed out waiting for connection");
                    return u64::MAX;
                }
            }

            // Fill in the peer address if requested.
            if want_addr {
                let out = unsafe { core::slice::from_raw_parts_mut(a1 as *mut u8, a2 as usize) };
                // Zero the family/port/addr fields we touch.
                for b in out.iter_mut().take(8) {
                    *b = 0;
                }
                out[0] = 2; // AF_INET
                if let Some((ip, port)) = crate::net::stack::tcp_remote_endpoint(socket_id) {
                    out[2] = (port >> 8) as u8;
                    out[3] = (port & 0xFF) as u8;
                    out[4..8].copy_from_slice(&ip);
                }
            }

            // Return the same fd: it now refers to the established connection.
            sockfd as u64
        }
        SYS_SHUTDOWN => {
            // a0 = sockfd, a1 = how (ignored; we always close both directions).
            let sockfd = a0 as u32;
            let task_id = task::current_id() as u32;
            match get_socket_id(task_id, sockfd) {
                Some(socket_id) => {
                    crate::net::stack::tcp_close(socket_id);
                    0
                }
                None => u64::MAX,
            }
        }
        SYS_CONNECT => {
            // a0 = sockfd, a1 = addr_ptr, a2 = addrlen.
            // Returns 0 on success, -1 on error.
            let sockfd = a0 as u32;
            let task_id = task::current_id() as u32;
            
            // Validate user pointer
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User && !is_user_pointer(a1, a2 as usize) {
                return u64::MAX;
            }
            
            if a2 < 8 {
                return u64::MAX;
            }
            
            let addr_bytes = unsafe { core::slice::from_raw_parts(a1 as *const u8, a2 as usize) };
            let port = u16::from_be_bytes([addr_bytes[2], addr_bytes[3]]);
            let ip = smoltcp::wire::Ipv4Address::new(
                addr_bytes[4], addr_bytes[5], addr_bytes[6], addr_bytes[7]
            );
            
            match get_socket_id(task_id, sockfd) {
                Some(socket_id) => {
                    // Use ephemeral port for local
                    let local_port = 49152 + (socket_id as u16 % 10000);
                    match crate::net::stack::tcp_connect(socket_id, ip, port, local_port) {
                        Ok(_) => 0,
                        Err(e) => {
                            serial_println!("[syscall] SYS_CONNECT: failed: {}", e);
                            u64::MAX
                        }
                    }
                }
                None => u64::MAX,
            }
        }
        SYS_SEND => {
            // a0 = sockfd, a1 = buf_ptr, a2 = len, a3 = flags (ignored).
            // Returns bytes sent or -1 on error.
            let sockfd = a0 as u32;
            let len = a2 as usize;
            let task_id = task::current_id() as u32;
            
            // Validate user pointer
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User && !is_user_pointer(a1, len) {
                return u64::MAX;
            }
            
            let data = unsafe { core::slice::from_raw_parts(a1 as *const u8, len) };
            
            match get_socket_id(task_id, sockfd) {
                Some(socket_id) => {
                    match crate::net::stack::tcp_send(socket_id, data) {
                        Ok(n) => n as u64,
                        Err(e) => {
                            serial_println!("[syscall] SYS_SEND: failed: {}", e);
                            u64::MAX
                        }
                    }
                }
                None => u64::MAX,
            }
        }
        SYS_RECV => {
            // a0 = sockfd, a1 = buf_ptr, a2 = len, a3 = flags (ignored).
            // Returns bytes received or -1 on error.
            let sockfd = a0 as u32;
            let len = a2 as usize;
            let task_id = task::current_id() as u32;
            
            // Validate user pointer
            let current_ring = task::scheduler::with(|s| s.tasks[s.current].ring);
            if current_ring == task::Ring::User && !is_user_pointer(a1, len) {
                return u64::MAX;
            }
            
            let buffer = unsafe { core::slice::from_raw_parts_mut(a1 as *mut u8, len) };
            
            match get_socket_id(task_id, sockfd) {
                Some(socket_id) => {
                    match crate::net::stack::tcp_recv(socket_id, buffer) {
                        Ok(n) => n as u64,
                        Err(e) => {
                            serial_println!("[syscall] SYS_RECV: failed: {}", e);
                            u64::MAX
                        }
                    }
                }
                None => u64::MAX,
            }
        }
        other => {
            serial_println!("[syscall] unknown syscall {}", other);
            u64::MAX // -1: error
        }
    }
}

/// Convert raw flags to OpenFlags.
fn sys_open_flags_from_raw(raw: u64) -> crate::fs::OpenFlags {
    const O_RDONLY: u64 = 0x0;
    const O_WRONLY: u64 = 0x1;
    const O_RDWR: u64 = 0x2;
    const O_CREAT: u64 = 0x40;
    const O_TRUNC: u64 = 0x200;
    const O_APPEND: u64 = 0x400;

    let access_mode = raw & 0x3;
    let read = access_mode == O_RDONLY || access_mode == O_RDWR;
    let write = access_mode == O_WRONLY || access_mode == O_RDWR;
    let create = (raw & O_CREAT) != 0;
    let truncate = (raw & O_TRUNC) != 0;
    let append = (raw & O_APPEND) != 0;

    crate::fs::OpenFlags {
        read,
        write,
        create,
        truncate,
        append,
    }
}

// ---------------------------------------------------------------------------
// Thin Rust wrappers so kernel tasks (and, later, a user-space libc) can issue
// syscalls ergonomically. These execute the real `int 0x80` instruction, so
// they exercise the exact same path user code would.
// ---------------------------------------------------------------------------

/// Issue a syscall with up to three arguments.
///
/// # Safety
/// The arguments must be valid for the requested syscall (e.g. `SYS_PRINT`
/// requires a valid (ptr,len) pair).
#[inline(always)]
pub unsafe fn syscall3(num: u64, a0: u64, a1: u64, a2: u64) -> u64 {
    let ret: u64;
    asm!(
        "int 0x80",
        inlateout("rax") num => ret,
        in("rdi") a0,
        in("rsi") a1,
        in("rdx") a2,
        // The stub preserves these, but mark them clobbered to be safe.
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack),
    );
    ret
}

/// Issue a syscall with up to four arguments (the 4th goes in `r10`, matching
/// the kernel's `SYS_EXEC`/`SYS_WIN_CREATE` ABI).
///
/// # Safety
/// The arguments must be valid for the requested syscall.
#[inline(always)]
pub unsafe fn syscall4(num: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret: u64;
    asm!(
        "int 0x80",
        inlateout("rax") num => ret,
        in("rdi") a0,
        in("rsi") a1,
        in("rdx") a2,
        in("r10") a3,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack),
    );
    ret
}

/// Convenience: create a window via `SYS_WIN_CREATE`. Returns the window id, or
/// `u64::MAX` on failure.
pub fn sys_win_create(w: u32, h: u32, title: &str) -> u64 {
    unsafe {
        syscall4(
            SYS_WIN_CREATE,
            w as u64,
            h as u64,
            title.as_ptr() as u64,
            title.len() as u64,
        )
    }
}

/// Convenience: present a pixel buffer to a window via `SYS_WIN_COMMIT`.
pub fn sys_win_commit(win_id: u64, pixels: &[u32]) -> u64 {
    unsafe {
        syscall3(
            SYS_WIN_COMMIT,
            win_id,
            pixels.as_ptr() as u64,
            (pixels.len() * 4) as u64,
        )
    }
}

/// Convenience: poll one input event for a window via `SYS_WIN_POLL`. Returns
/// `1` if an event was written into `out`, `0` if none, `u64::MAX` on error.
pub fn sys_win_poll(win_id: u64, out: &mut [u8; 16]) -> u64 {
    unsafe { syscall3(SYS_WIN_POLL, win_id, out.as_mut_ptr() as u64, 0) }
}

/// Convenience: destroy a window via `SYS_WIN_DESTROY`.
pub fn sys_win_destroy(win_id: u64) -> u64 {
    unsafe { syscall3(SYS_WIN_DESTROY, win_id, 0, 0) }
}

/// Convenience: print a string via `SYS_PRINT`.
pub fn sys_print(s: &str) {
    unsafe {
        syscall3(SYS_PRINT, s.as_ptr() as u64, s.len() as u64, 0);
    }
}

/// Convenience: get the current task id via `SYS_GETPID`.
pub fn sys_getpid() -> u64 {
    unsafe { syscall3(SYS_GETPID, 0, 0, 0) }
}

/// Convenience: timer ticks since boot via `SYS_TICKS`.
pub fn sys_ticks() -> u64 {
    unsafe { syscall3(SYS_TICKS, 0, 0, 0) }
}

/// Convenience: get the parent task id via `SYS_GETPPID`.
pub fn sys_getppid() -> u64 {
    unsafe { syscall3(SYS_GETPPID, 0, 0, 0) }
}

/// Convenience: send a signal to a task via `SYS_KILL`.
pub fn sys_kill(pid: u64, sig: u32) -> u64 {
    unsafe { syscall3(SYS_KILL, pid, sig as u64, 0) }
}

/// Convenience: install a signal handler via `SYS_SIGNAL`.
pub fn sys_signal(sig: u32, handler: u64, restorer: u64) -> u64 {
    unsafe { syscall3(SYS_SIGNAL, sig as u64, handler, restorer) }
}
