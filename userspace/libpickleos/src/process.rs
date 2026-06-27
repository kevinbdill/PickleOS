//! Process management abstractions for PICKLE OS.

use crate::syscall;

/// Get the current process ID.
pub fn getpid() -> u64 {
    syscall::sys_getpid()
}

/// Get the parent process ID (0 if none).
pub fn getppid() -> u64 {
    syscall::sys_getppid()
}

/// Send signal `sig` to process `pid`. Returns true on success.
pub fn kill(pid: u64, sig: u32) -> bool {
    syscall::sys_kill(pid, sig)
}

/// Install a signal handler (see [`syscall::sys_signal`]).
pub fn signal(sig: u32, handler: u64, restorer: u64) -> u64 {
    syscall::sys_signal(sig, handler, restorer)
}

/// Exit the current process.
pub fn exit(code: i32) -> ! {
    syscall::sys_exit(code)
}

/// Yield the CPU to the scheduler.
pub fn yield_now() {
    syscall::sys_yield();
}

/// Sleep for the specified number of ticks.
pub fn sleep(ticks: u64) {
    syscall::sys_sleep(ticks);
}

/// Spawn a new process from an ELF binary.
pub fn spawn(binary: &[u8], name: &str) -> Result<u64, &'static str> {
    syscall::sys_spawn(binary, name)
}

/// Fork the current process. Returns child pid in parent, 0 in child,
/// or `None` on error.
pub fn fork() -> Option<u64> {
    let r = syscall::sys_fork();
    if r == u64::MAX { None } else { Some(r) }
}

/// Replace the current process image with the program at `path`.
/// Returns only on failure.
pub fn exec(path: &str) -> bool {
    syscall::sys_exec(path) != u64::MAX
}

/// Wait for any child to terminate. Returns the reaped child's pid, or
/// `None` if there are no children.
pub fn wait() -> Option<u64> {
    let r = syscall::sys_wait();
    if r == u64::MAX { None } else { Some(r) }
}

/// Launch a program from a filesystem path (fork + exec). Returns the child
/// pid in the parent, or `None` on error.
pub fn spawn_path(path: &str) -> Option<u64> {
    let r = syscall::sys_spawn_path(path);
    if r == u64::MAX { None } else { Some(r) }
}
