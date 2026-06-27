//! POSIX-style signal definitions and per-task signal state.
//!
//! PICKLE OS implements a small but real signal facility:
//!
//!   * Every task carries a pending-signal bitmask and a table of per-signal
//!     user handlers (see [`SignalState`]).
//!   * `kill(pid, sig)` (the `SYS_KILL` syscall) either applies the signal's
//!     *default action* immediately (terminate, or ignore) or, when the target
//!     installed a custom handler, marks the signal pending.
//!   * Pending caught signals are delivered at the kernel/user boundary: when
//!     the target task next returns from a syscall, [`crate::task`] rewrites its
//!     trap frame so execution resumes inside the user handler. A small
//!     user-space trampoline then issues `SYS_SIGRETURN`, which restores the
//!     interrupted context.
//!
//! This mirrors how classic Unix kernels deliver signals (on syscall/interrupt
//! return) while staying simple enough to follow end-to-end. Asynchronous
//! delivery from the timer interrupt is intentionally out of scope; a caught
//! signal is delivered the next time the target crosses a syscall boundary.

/// Number of distinct signal numbers supported (1..=NSIG-1 are valid).
pub const NSIG: usize = 32;

// --- Supported signal numbers (Linux-compatible values) --------------------
/// Hangup (default: terminate).
pub const SIGHUP: u32 = 1;
/// Interrupt, e.g. Ctrl-C (default: terminate).
pub const SIGINT: u32 = 2;
/// Kill — cannot be caught or ignored (default: terminate).
pub const SIGKILL: u32 = 9;
/// User-defined signal 1 (default: terminate).
pub const SIGUSR1: u32 = 10;
/// User-defined signal 2 (default: terminate).
pub const SIGUSR2: u32 = 12;
/// Termination request (default: terminate, but catchable).
pub const SIGTERM: u32 = 15;
/// Child stopped or terminated (default: ignore).
pub const SIGCHLD: u32 = 17;

// --- Special handler values ------------------------------------------------
/// Default action: handler value meaning "use the signal's default behaviour".
pub const SIG_DFL: u64 = 0;
/// Ignore: handler value meaning "discard the signal".
pub const SIG_IGN: u64 = 1;

/// Returns `true` if the default action for `sig` is to terminate the process.
/// Signals whose default action is "ignore" (e.g. `SIGCHLD`) return `false`.
pub fn default_terminates(sig: u32) -> bool {
    !matches!(sig, SIGCHLD)
}

/// Saved user context captured when a signal handler is invoked, used by
/// `SYS_SIGRETURN` to resume the interrupted instruction stream.
#[derive(Debug, Clone, Copy)]
pub struct SavedSigContext {
    /// Instruction pointer to resume at after the handler returns.
    pub rip: u64,
    /// Saved RFLAGS.
    pub rflags: u64,
    /// User stack pointer to restore.
    pub rsp: u64,
    /// Value the interrupted syscall would have returned in `rax`.
    pub rax: u64,
}

/// Per-task signal bookkeeping.
#[derive(Clone)]
pub struct SignalState {
    /// Bitmask of pending (caught) signals awaiting delivery.
    pub pending: u32,
    /// Per-signal disposition: [`SIG_DFL`], [`SIG_IGN`], or a user handler
    /// address. Index by signal number (0 is unused).
    pub handlers: [u64; NSIG],
    /// User-space "restorer" trampoline that issues `SYS_SIGRETURN`. Supplied
    /// by libc when a handler is installed.
    pub restorer: u64,
    /// Saved context while a handler is executing (None when not in a handler).
    pub saved: Option<SavedSigContext>,
}

impl SignalState {
    /// A fresh signal state: nothing pending, all dispositions default.
    pub const fn new() -> SignalState {
        SignalState {
            pending: 0,
            handlers: [SIG_DFL; NSIG],
            restorer: 0,
            saved: None,
        }
    }

    /// Reset dispositions to default (used on `exec`). Caught handlers do not
    /// survive an `execve`, but ignored signals stay ignored on real Unix; for
    /// simplicity we reset everything to the default action here.
    pub fn reset_for_exec(&mut self) {
        self.pending = 0;
        self.handlers = [SIG_DFL; NSIG];
        self.restorer = 0;
        self.saved = None;
    }

    /// Mark `sig` pending.
    pub fn set_pending(&mut self, sig: u32) {
        if (sig as usize) < NSIG {
            self.pending |= 1 << sig;
        }
    }

    /// Clear `sig` from the pending set.
    pub fn clear_pending(&mut self, sig: u32) {
        if (sig as usize) < NSIG {
            self.pending &= !(1 << sig);
        }
    }

    /// Return the lowest-numbered pending signal that has a custom handler
    /// installed, or `None`. Also returns the handler address.
    pub fn next_deliverable(&self) -> Option<(u32, u64)> {
        for sig in 1..NSIG as u32 {
            if self.pending & (1 << sig) != 0 {
                let h = self.handlers[sig as usize];
                if h != SIG_DFL && h != SIG_IGN {
                    return Some((sig, h));
                }
            }
        }
        None
    }
}

impl Default for SignalState {
    fn default() -> Self {
        SignalState::new()
    }
}
