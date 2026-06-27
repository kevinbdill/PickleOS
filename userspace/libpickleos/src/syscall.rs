//! System call wrappers for PICKLE OS.
//!
//! These functions provide a safe(r) Rust API around the raw `int 0x80` syscall interface.

use core::arch::asm;

// Syscall numbers (must match kernel definitions)
pub const SYS_PRINT: u64 = 1;
pub const SYS_GETPID: u64 = 2;
pub const SYS_EXIT: u64 = 3;
pub const SYS_YIELD: u64 = 4;
pub const SYS_TICKS: u64 = 5;
pub const SYS_IPC_SEND: u64 = 6;
pub const SYS_IPC_RECV: u64 = 7;
pub const SYS_CAP_CHECK: u64 = 8;
pub const SYS_SLEEP: u64 = 9;
pub const SYS_SPAWN: u64 = 10;
pub const SYS_MMAP: u64 = 11;
pub const SYS_MUNMAP: u64 = 12;
pub const SYS_GETPPID: u64 = 30;
pub const SYS_KILL: u64 = 31;
pub const SYS_SIGNAL: u64 = 32;
pub const SYS_SIGRETURN: u64 = 33;
// Window-server syscalls (see kernel `wm` module / display server design doc).
pub const SYS_WIN_CREATE: u64 = 34; // (w, h, title_ptr, title_len[r10]) -> win_id or -1
pub const SYS_WIN_COMMIT: u64 = 35; // (win_id, buf_ptr, byte_len) -> 0 or -1
pub const SYS_WIN_POLL: u64 = 36; // (win_id, event_ptr[16]) -> 1 got / 0 none / -1 err
pub const SYS_WIN_DESTROY: u64 = 37; // (win_id) -> 0 or -1
pub const SYS_WIN_INFO: u64 = 38; // (win_id, info_ptr[16]) -> 0 or -1
// Networking syscalls
pub const SYS_SOCKET: u64 = 39; // (domain, type, protocol) -> fd or -1
pub const SYS_BIND: u64 = 40; // (sockfd, addr_ptr, addrlen) -> 0 or -1
pub const SYS_CONNECT: u64 = 41; // (sockfd, addr_ptr, addrlen) -> 0 or -1
pub const SYS_LISTEN: u64 = 42; // (sockfd, backlog) -> 0 or -1
pub const SYS_ACCEPT: u64 = 43; // (sockfd, addr_ptr, addrlen_ptr) -> new_fd or -1
pub const SYS_SEND: u64 = 44; // (sockfd, buf_ptr, len, flags) -> bytes_sent or -1
pub const SYS_RECV: u64 = 45; // (sockfd, buf_ptr, len, flags) -> bytes_recv or -1
pub const SYS_SHUTDOWN: u64 = 46; // (sockfd, how) -> 0 or -1

// File I/O constants
pub const O_RDONLY: u64 = 0;
pub const O_WRONLY: u64 = 1;
pub const O_RDWR: u64 = 2;
pub const O_CREAT: u64 = 0x40;
pub const O_TRUNC: u64 = 0x200;

// Signal numbers (must match kernel `signal` module).
pub const SIGHUP: u32 = 1;
pub const SIGINT: u32 = 2;
pub const SIGKILL: u32 = 9;
pub const SIGUSR1: u32 = 10;
pub const SIGUSR2: u32 = 12;
pub const SIGTERM: u32 = 15;
pub const SIGCHLD: u32 = 17;
/// Default signal disposition (terminate or ignore per signal).
pub const SIG_DFL: u64 = 0;
/// Ignore the signal.
pub const SIG_IGN: u64 = 1;

/// Low-level syscall with up to 3 arguments.
#[inline(always)]
pub unsafe fn syscall3(num: u64, a0: u64, a1: u64, a2: u64) -> u64 {
    let ret: u64;
    asm!(
        "int 0x80",
        inlateout("rax") num => ret,
        in("rdi") a0,
        in("rsi") a1,
        in("rdx") a2,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack),
    );
    ret
}

/// Low-level syscall with 4 arguments (the 4th is passed in `r10`, matching the
/// kernel `SYS_WIN_CREATE`/`SYS_EXEC` ABI).
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

/// Create a window via `SYS_WIN_CREATE`. Returns the window id, or `u64::MAX`
/// on failure.
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

/// Present a pixel buffer (`0x00RRGGBB` per pixel) to a window via
/// `SYS_WIN_COMMIT`. Returns `0` on success, `u64::MAX` on failure.
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

/// Poll one input event for a window via `SYS_WIN_POLL`. Returns `1` if an
/// event was written into `out`, `0` if the queue was empty, `u64::MAX` on
/// error.
pub fn sys_win_poll(win_id: u64, out: &mut [u8; 16]) -> u64 {
    unsafe { syscall3(SYS_WIN_POLL, win_id, out.as_mut_ptr() as u64, 0) }
}

/// Destroy a window via `SYS_WIN_DESTROY`. Returns `0` on success.
pub fn sys_win_destroy(win_id: u64) -> u64 {
    unsafe { syscall3(SYS_WIN_DESTROY, win_id, 0, 0) }
}

/// Query a window's geometry via `SYS_WIN_INFO`. On success fills `out` with
/// four little-endian fields: u32 w, u32 h, i32 x, i32 y. Returns `0` on
/// success, `u64::MAX` on failure.
pub fn sys_win_info(win_id: u64, out: &mut [u8; 16]) -> u64 {
    unsafe { syscall3(SYS_WIN_INFO, win_id, out.as_mut_ptr() as u64, 0) }
}

/// Print a string to the console.
pub fn sys_print(s: &str) {
    unsafe {
        syscall3(SYS_PRINT, s.as_ptr() as u64, s.len() as u64, 0);
    }
}

/// Get the current process ID.
pub fn sys_getpid() -> u64 {
    unsafe { syscall3(SYS_GETPID, 0, 0, 0) }
}

/// Get the parent process ID (0 if none).
pub fn sys_getppid() -> u64 {
    unsafe { syscall3(SYS_GETPPID, 0, 0, 0) }
}

/// Send signal `sig` to process `pid`. Returns true on success.
pub fn sys_kill(pid: u64, sig: u32) -> bool {
    unsafe { syscall3(SYS_KILL, pid, sig as u64, 0) != u64::MAX }
}

/// Install a signal handler. `handler` is a user function pointer (or
/// `SIG_DFL`/`SIG_IGN`); `restorer` is the trampoline that issues
/// `SYS_SIGRETURN` when the handler returns. Returns the previous handler, or
/// `u64::MAX` on error.
pub fn sys_signal(sig: u32, handler: u64, restorer: u64) -> u64 {
    unsafe { syscall3(SYS_SIGNAL, sig as u64, handler, restorer) }
}

/// Exit the current process with status code.
pub fn sys_exit(code: i32) -> ! {
    unsafe {
        syscall3(SYS_EXIT, code as u64, 0, 0);
    }
    loop {}
}

/// Yield CPU to the scheduler.
pub fn sys_yield() {
    unsafe {
        syscall3(SYS_YIELD, 0, 0, 0);
    }
}

/// Get the system tick count.
pub fn sys_ticks() -> u64 {
    unsafe { syscall3(SYS_TICKS, 0, 0, 0) }
}

/// Send an IPC message to an endpoint.
pub fn sys_ipc_send(endpoint: u64, tag: u64) {
    unsafe {
        syscall3(SYS_IPC_SEND, endpoint, tag, 0);
    }
}

/// Receive an IPC message from an endpoint (blocks until message arrives).
pub fn sys_ipc_recv(endpoint: u64) -> u64 {
    unsafe { syscall3(SYS_IPC_RECV, endpoint, 0, 0) }
}

/// Check if the current task has a capability with the specified rights.
pub fn sys_cap_check(slot: usize, rights: u32) -> bool {
    unsafe { syscall3(SYS_CAP_CHECK, slot as u64, rights as u64, 0) != 0 }
}

/// Sleep for the specified number of ticks.
pub fn sys_sleep(ticks: u64) {
    unsafe {
        syscall3(SYS_SLEEP, ticks, 0, 0);
    }
}

/// Spawn a new process from an ELF binary.
/// Returns the task ID on success, or an error.
pub fn sys_spawn(binary: &[u8], name: &str) -> Result<u64, &'static str> {
    let name_bytes = name.as_bytes();
    let result = unsafe {
        syscall3(
            SYS_SPAWN,
            binary.as_ptr() as u64,
            binary.len() as u64,
            name_bytes.as_ptr() as u64,
        )
    };
    
    if result == u64::MAX {
        Err("spawn failed")
    } else {
        Ok(result)
    }
}

/// Map anonymous memory.
/// Returns the mapped address on success.
pub fn sys_mmap(addr: u64, len: usize, prot: u32) -> Result<usize, &'static str> {
    let result = unsafe { syscall3(SYS_MMAP, addr, len as u64, prot as u64) };
    
    if result == u64::MAX {
        Err("mmap failed")
    } else {
        Ok(result as usize)
    }
}

/// Unmap memory.
pub fn sys_munmap(addr: u64, len: usize) -> Result<(), &'static str> {
    let result = unsafe { syscall3(SYS_MUNMAP, addr, len as u64, 0) };
    
    if result == u64::MAX {
        Err("munmap failed")
    } else {
        Ok(())
    }
}



// --- Socket syscalls ---

/// Create a socket
pub fn sys_socket(domain: u64, ty: u64, protocol: u64) -> Result<u32, &'static str> {
    let result = unsafe { syscall3(SYS_SOCKET, domain, ty, protocol) };
    if result == u64::MAX {
        Err("socket creation failed")
    } else {
        Ok(result as u32)
    }
}

/// Bind a socket to an address
pub fn sys_bind(sockfd: u32, addr: &[u8]) -> Result<(), &'static str> {
    let result = unsafe {
        syscall3(
            SYS_BIND,
            sockfd as u64,
            addr.as_ptr() as u64,
            addr.len() as u64,
        )
    };
    if result == u64::MAX {
        Err("bind failed")
    } else {
        Ok(())
    }
}

/// Connect to a remote address
pub fn sys_connect(sockfd: u32, addr: &[u8]) -> Result<(), &'static str> {
    let result = unsafe {
        syscall3(
            SYS_CONNECT,
            sockfd as u64,
            addr.as_ptr() as u64,
            addr.len() as u64,
        )
    };
    if result == u64::MAX {
        Err("connect failed")
    } else {
        Ok(())
    }
}

/// Listen for connections
pub fn sys_listen(sockfd: u32, backlog: u32) -> Result<(), &'static str> {
    let result = unsafe { syscall3(SYS_LISTEN, sockfd as u64, backlog as u64, 0) };
    if result == u64::MAX {
        Err("listen failed")
    } else {
        Ok(())
    }
}

/// Send data
pub fn sys_send(sockfd: u32, data: &[u8], flags: u32) -> Result<usize, &'static str> {
    let result = unsafe {
        syscall3(
            SYS_SEND,
            sockfd as u64,
            data.as_ptr() as u64,
            data.len() as u64,
        )
    };
    if result == u64::MAX {
        Err("send failed")
    } else {
        Ok(result as usize)
    }
}

/// Receive data
pub fn sys_recv(sockfd: u32, buffer: &mut [u8], flags: u32) -> Result<usize, &'static str> {
    let _ = flags;
    let result = unsafe {
        syscall3(
            SYS_RECV,
            sockfd as u64,
            buffer.as_mut_ptr() as u64,
            buffer.len() as u64,
        )
    };
    if result == u64::MAX {
        Err("recv failed")
    } else {
        Ok(result as usize)
    }
}

/// Accept a connection on a listening socket.
///
/// Blocks until a peer connects. Returns the connected socket fd (in the
/// current simplified model this is the same fd that was listening). If
/// `addr` is provided it is filled with the peer's `sockaddr_in` bytes.
pub fn sys_accept(sockfd: u32, addr: Option<&mut [u8]>) -> Result<u32, &'static str> {
    let (ptr, len) = match addr {
        Some(buf) => (buf.as_mut_ptr() as u64, buf.len() as u64),
        None => (0u64, 0u64),
    };
    let result = unsafe { syscall3(SYS_ACCEPT, sockfd as u64, ptr, len) };
    if result == u64::MAX {
        Err("accept failed")
    } else {
        Ok(result as u32)
    }
}

/// Shut down a socket connection.
pub fn sys_shutdown(sockfd: u32, how: u32) -> Result<(), &'static str> {
    let result = unsafe { syscall3(SYS_SHUTDOWN, sockfd as u64, how as u64, 0) };
    if result == u64::MAX {
        Err("shutdown failed")
    } else {
        Ok(())
    }
}


// File I/O syscalls
const SYS_OPEN: u64 = 13;
const SYS_READ: u64 = 14;
const SYS_WRITE: u64 = 15;
const SYS_CLOSE: u64 = 16;
const SYS_LSEEK: u64 = 17;
const SYS_UNLINK: u64 = 18;
const SYS_RMDIR: u64 = 19;
const SYS_MKDIR: u64 = 20;
const SYS_STAT: u64 = 24;

/// Open or create a file. Returns file descriptor or u64::MAX on error.
pub fn sys_open(path: &str, flags: u64) -> u64 {
    unsafe {
        syscall3(
            SYS_OPEN,
            path.as_ptr() as u64,
            path.len() as u64,
            flags,
        )
    }
}

/// Read from a file descriptor into a buffer. Returns bytes read or u64::MAX on error.
pub fn sys_read(fd: u32, buf: &mut [u8]) -> u64 {
    unsafe {
        syscall3(
            SYS_READ,
            fd as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        )
    }
}

/// Write to a file descriptor from a buffer. Returns bytes written or u64::MAX on error.
pub fn sys_write(fd: u32, buf: &[u8]) -> u64 {
    unsafe {
        syscall3(
            SYS_WRITE,
            fd as u64,
            buf.as_ptr() as u64,
            buf.len() as u64,
        )
    }
}

/// Close a file descriptor. Returns 0 on success or u64::MAX on error.
pub fn sys_close(fd: u32) -> u64 {
    unsafe {
        syscall3(SYS_CLOSE, fd as u64, 0, 0)
    }
}

/// Create a directory. Returns 0 on success or u64::MAX on error.
pub fn sys_mkdir(path: &str) -> u64 {
    unsafe {
        syscall3(
            SYS_MKDIR,
            path.as_ptr() as u64,
            path.len() as u64,
            0,
        )
    }
}

/// Remove a file. Returns 0 on success or u64::MAX on error.
pub fn sys_unlink(path: &str) -> u64 {
    unsafe {
        syscall3(
            SYS_UNLINK,
            path.as_ptr() as u64,
            path.len() as u64,
            0,
        )
    }
}

/// Remove an empty directory. Returns 0 on success or u64::MAX on error.
pub fn sys_rmdir(path: &str) -> u64 {
    unsafe {
        syscall3(
            SYS_RMDIR,
            path.as_ptr() as u64,
            path.len() as u64,
            0,
        )
    }
}

/// Stat a file or directory. Fills a 24-byte buffer with [size: u64, is_dir: u8, _: 15 bytes].
/// Returns 0 on success or u64::MAX on error.
pub fn sys_stat(path: &str, statbuf: &mut [u8; 24]) -> u64 {
    unsafe {
        syscall3(
            SYS_STAT,
            path.as_ptr() as u64,
            path.len() as u64,
            statbuf.as_mut_ptr() as u64,
        )
    }
}


// Process-management syscalls (fork/exec/wait) for the launcher.
const SYS_EXIT2: u64 = 25;
const SYS_WAIT: u64 = 26;
const SYS_EXEC: u64 = 27;
const SYS_FORK: u64 = 28;

/// Fork the current process. Returns the child pid in the parent, 0 in the
/// child, or u64::MAX on error.
pub fn sys_fork() -> u64 {
    unsafe { syscall3(SYS_FORK, 0, 0, 0) }
}

/// Replace the current process image with the program at `path`. On success
/// this does not return; on failure it returns u64::MAX. `argv`/`envp` are
/// passed as NULL (0) in this simplified wrapper.
pub fn sys_exec(path: &str) -> u64 {
    unsafe {
        syscall4(
            SYS_EXEC,
            path.as_ptr() as u64,
            path.len() as u64,
            0,
            0,
        )
    }
}

/// Wait for a child process to terminate. Returns the child pid, or u64::MAX
/// if there are no children. The exit status is ignored in this wrapper.
pub fn sys_wait() -> u64 {
    unsafe { syscall3(SYS_WAIT, 0, 0, 0) }
}

/// Spawn a program from a filesystem path by forking and exec'ing. Returns the
/// child pid in the parent on success, or u64::MAX on error. The child never
/// returns from this function (it either exec's or exits).
pub fn sys_spawn_path(path: &str) -> u64 {
    let pid = sys_fork();
    if pid == 0 {
        // Child: replace image with the target program.
        sys_exec(path);
        // exec only returns on failure.
        sys_exit(1);
    }
    pid
}
