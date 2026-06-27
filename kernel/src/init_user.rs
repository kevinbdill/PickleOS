//! User-space `init`: the first program launcher.
//!
//! After NextFS is mounted globally, the kernel hands control to this module to
//! bring up user space the way a real OS would:
//!
//!   1. Seed a minimal root filesystem layout (`/bin`, `/etc`) and populate
//!      `/bin/hello` and `/bin/test_lib` from the kernel's embedded ELF images.
//!   2. Write `/etc/inittab`, a plain-text manifest listing the programs that
//!      `init` should launch at boot (one `name:path` entry per line).
//!   3. Parse `/etc/inittab` and spawn each listed program straight from disk
//!      using [`crate::task::spawn_user_task_from_file`].
//!
//! Everything here is intentionally defensive: a failure to seed or spawn any
//! single program is logged and skipped so the rest of boot still proceeds.
//! All filesystem access goes through the VFS as task 0 (root credentials).

use crate::fs::{self, OpenFlags};
use crate::serial_println;

/// Task id used for init's filesystem operations (boot context, root creds).
const INIT_TASK: u32 = 0;

/// The inittab manifest path.
const INITTAB_PATH: &str = "/etc/inittab";

/// Programs init seeds onto NextFS and the inittab it writes. Each tuple is
/// `(absolute path, embedded ELF image)`.
const SEED_PROGRAMS: &[(&str, &[u8])] = &[
    ("/bin/hello", crate::userprogs::HELLO),
    ("/bin/test_lib", crate::userprogs::TEST_LIB),
    ("/bin/fork_test", crate::userprogs::FORK_TEST),
    ("/bin/pipe_test", crate::userprogs::PIPE_TEST),
    ("/bin/stdio_test", crate::userprogs::STDIO_TEST),
    ("/bin/args_test", crate::userprogs::ARGS_TEST),
    ("/bin/signal_test", crate::userprogs::SIGNAL_TEST),
    ("/bin/pid_test", crate::userprogs::PID_TEST),
    ("/bin/calculator", crate::userprogs::CALCULATOR),
    ("/bin/net_test", crate::userprogs::NET_TEST),
    ("/bin/pie_test", crate::userprogs::PIE_TEST),
    ("/bin/filemanager", crate::userprogs::FILEMANAGER),
    ("/bin/texteditor", crate::userprogs::TEXTEDITOR),
    ("/bin/launcher", crate::userprogs::LAUNCHER),
    ("/bin/taskbar", crate::userprogs::TASKBAR),
];

/// Default inittab contents: launch the seeded programs. Format is one
/// `name:path` entry per line; `#` begins a comment.
///
/// Ordering note: programs that `exec()` a child (`fork_test`, `args_test`)
/// are listed last. Their children re-read binaries from NextFS, and doing so
/// concurrently with init's own sequential launch reads can race the
/// (currently single-threaded) NextFS lookup path, so we finish all the simple
/// launches before kicking off the exec-heavy ones. `net_test` is a simple
/// (non-exec) launch, so it is grouped with the simple programs.
const DEFAULT_INITTAB: &[u8] = b"# PICKLE OS inittab - programs launched by init at boot\n\
# format: <name>:<absolute-path>\n\
# GUI shell first so the desktop (taskbar + launcher) is up immediately.\n\
taskbar:/bin/taskbar\n\
launcher:/bin/launcher\n\
hello:/bin/hello\n\
test_lib:/bin/test_lib\n\
pid_test:/bin/pid_test\n\
signal_test:/bin/signal_test\n\
pipe_test:/bin/pipe_test\n\
net_test:/bin/net_test\n\
pie_test:/bin/pie_test\n";
// NOTE: `filemanager`, `texteditor`, `calculator`, `fork_test` and `args_test`
// are seeded into /bin and launchable from the desktop Launcher, but are NOT
// auto-started at boot. The exec-heavy tests (`fork_test`/`args_test`) hammer
// the currently single-threaded NextFS lookup path concurrently with init's
// own sequential launches, and `calculator`/`filemanager` have known runtime
// faults; keeping them out of the boot path keeps the desktop bring-up stable.
// (User-mode faults no longer take down the kernel — see the page-fault /
// GP / invalid-opcode handlers in `interrupts.rs` — so launching them from the
// Launcher fails gracefully instead of panicking.)

/// Run the user-space bring-up sequence. Called once, after the global NextFS
/// mount succeeds. Never panics: all errors are logged and tolerated.
pub fn run() {
    serial_println!("[init-user] === user-space init starting ===");

    // Make sure init's task has standard file descriptors wired up.
    fs::init_task_fds(INIT_TASK);

    seed_filesystem();
    let launched = launch_from_inittab();

    serial_println!("[init-user] === user-space init complete ({} program(s) launched) ===", launched);

    // Now that NextFS is mounted and seeded, run the shell pipe/redirect
    // self-test. The interactive shell renders only to VGA (invisible in a
    // headless serial boot), so this drives the `|` and `>` machinery directly
    // and reports results over serial with the `[selftest]` prefix.
    crate::shell::pipe_redirect_selftest();
}

/// Create the base directory layout and write the seed programs + inittab.
fn seed_filesystem() {
    // Create the standard directories (ignore "already exists").
    for dir in ["/bin", "/etc"] {
        match fs::mkdir(INIT_TASK, dir) {
            Ok(()) => serial_println!("[init-user] mkdir {}: OK", dir),
            Err(fs::VfsError::AlreadyExists) => {}
            Err(e) => serial_println!("[init-user] mkdir {} failed: {:?}", dir, e),
        }
    }

    // Copy each embedded program image onto the filesystem.
    for (path, image) in SEED_PROGRAMS {
        match write_file(path, image) {
            Ok(n) => serial_println!("[init-user] seed {} : OK ({} bytes)", path, n),
            Err(e) => serial_println!("[init-user] seed {} FAILED: {:?}", path, e),
        }
    }

    // Write the inittab manifest.
    match write_file(INITTAB_PATH, DEFAULT_INITTAB) {
        Ok(_) => serial_println!("[init-user] wrote {}: OK", INITTAB_PATH),
        Err(e) => serial_println!("[init-user] write {} FAILED: {:?}", INITTAB_PATH, e),
    }
}

/// Create (or truncate) `path` and write the full contents of `data`.
fn write_file(path: &str, data: &[u8]) -> Result<usize, fs::VfsError> {
    let flags = OpenFlags::create().with_truncate();
    let fd = fs::open(INIT_TASK, path, flags)?;

    let mut total = 0;
    while total < data.len() {
        match fs::write(INIT_TASK, fd, &data[total..]) {
            Ok(0) => break, // no progress; avoid spinning forever
            Ok(n) => total += n,
            Err(e) => {
                let _ = fs::close(INIT_TASK, fd);
                return Err(e);
            }
        }
    }

    fs::close(INIT_TASK, fd)?;
    Ok(total)
}

/// Read `/etc/inittab`, parse it, and spawn each listed program from disk.
/// Returns the number of programs successfully launched.
fn launch_from_inittab() -> usize {
    // Read the whole inittab into a fixed buffer (it is tiny).
    let mut buf = [0u8; 1024];
    let n = match read_file(INITTAB_PATH, &mut buf) {
        Ok(n) => n,
        Err(e) => {
            serial_println!("[init-user] cannot read {}: {:?}", INITTAB_PATH, e);
            return 0;
        }
    };

    let text = match core::str::from_utf8(&buf[..n]) {
        Ok(t) => t,
        Err(_) => {
            serial_println!("[init-user] {} is not valid UTF-8", INITTAB_PATH);
            return 0;
        }
    };

    let mut launched = 0;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Parse "name:path".
        let (name, path) = match line.split_once(':') {
            Some((n, p)) => (n.trim(), p.trim()),
            None => {
                serial_println!("[init-user] skipping malformed inittab line: {:?}", line);
                continue;
            }
        };
        if name.is_empty() || path.is_empty() {
            serial_println!("[init-user] skipping malformed inittab line: {:?}", line);
            continue;
        }

        match crate::task::spawn_user_task_from_file(name, path) {
            Ok(id) => {
                serial_println!("[init-user] spawned '{}' from {} (task {})", name, path, id);
                launched += 1;
            }
            Err(e) => serial_println!("[init-user] spawn '{}' from {} FAILED: {}", name, path, e),
        }
    }

    launched
}

/// Open `path` read-only and read up to `buf.len()` bytes into `buf`.
fn read_file(path: &str, buf: &mut [u8]) -> Result<usize, fs::VfsError> {
    let fd = fs::open(INIT_TASK, path, OpenFlags::rdonly())?;
    let mut total = 0;
    while total < buf.len() {
        match fs::read(INIT_TASK, fd, &mut buf[total..]) {
            Ok(0) => break, // EOF
            Ok(n) => total += n,
            Err(e) => {
                let _ = fs::close(INIT_TASK, fd);
                return Err(e);
            }
        }
    }
    fs::close(INIT_TASK, fd)?;
    Ok(total)
}
