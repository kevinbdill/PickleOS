//! Embedded user-space programs.
//!
//! This module contains statically-embedded ELF binaries for user-space programs.
//! These are compiled separately and included at kernel build time.

/// The "hello" user program - a simple ring 3 test that makes syscalls.
pub static HELLO: &[u8] = include_bytes!("../../userspace/hello");

/// Test program for new syscalls (SYS_SLEEP, SYS_MMAP, etc.)
pub static TEST_LIB: &[u8] = include_bytes!("../../userspace/test_lib");

/// Process-management test: forks a child, the child exec's `/bin/hello`, and
/// the parent waits for and reaps it. Exercises fork/exec/wait/exit.
pub static FORK_TEST: &[u8] = include_bytes!("../../userspace/fork_test");

/// Pipe / IPC test: creates a pipe, forks, and passes a message from parent to
/// child through the kernel pipe buffer. Exercises SYS_PIPE + fork/wait/exit.
pub static PIPE_TEST: &[u8] = include_bytes!("../../userspace/pipe_test");

/// Stdio test: reads from stdin and echoes to stdout, testing blocking I/O.
pub static STDIO_TEST: &[u8] = include_bytes!("../../userspace/stdio_test");

/// argv/envp test: prints its own argument vector + environment, then forks a
/// child that re-exec's itself with a richer argv/envp to prove they cross the
/// exec boundary. Exercises argv/envp support in SYS_EXEC.
pub static ARGS_TEST: &[u8] = include_bytes!("../../userspace/args_test");

/// Signal test: installs a SIGUSR1 handler, signals a child (caught handler),
/// then terminates a second child with SIGTERM (default action). Exercises
/// SYS_KILL / SYS_SIGNAL / SYS_SIGRETURN.
pub static SIGNAL_TEST: &[u8] = include_bytes!("../../userspace/signal_test");

/// PID test: prints pid/ppid for parent and a forked child, verifying the
/// child's ppid matches the parent. Exercises SYS_GETPID / SYS_GETPPID.
pub static PID_TEST: &[u8] = include_bytes!("../../userspace/pid_test");
pub static CALCULATOR: &[u8] = include_bytes!("../../userspace/calculator");

/// Network test: exercises the TCP socket syscalls (socket/bind/listen/
/// shutdown server path, plus a best-effort connect/send/recv client path).
pub static NET_TEST: &[u8] = include_bytes!("../../userspace/net_test");
pub static PIE_TEST: &[u8] = include_bytes!("../../userspace/pie_test");
pub static FILEMANAGER: &[u8] = include_bytes!("../../userspace/filemanager_bin");

/// Text Editor: a multi-line text editor that loads/saves `/tmp/scratch.txt`.
/// Exercises file I/O plus GUI text rendering and keyboard input.
pub static TEXTEDITOR: &[u8] = include_bytes!("../../userspace/texteditor_bin");

/// Application Launcher: an icon grid that spawns the other GUI apps via
/// fork+exec. Exercises SYS_FORK / SYS_EXEC / SYS_WAIT from user space.
pub static LAUNCHER: &[u8] = include_bytes!("../../userspace/launcher_bin");

/// Taskbar: a horizontal system bar showing uptime/pid and quick-launch
/// buttons for the desktop apps.
pub static TASKBAR: &[u8] = include_bytes!("../../userspace/taskbar_bin");
