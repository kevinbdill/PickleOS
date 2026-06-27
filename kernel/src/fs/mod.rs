//! NextFS — a simple, Rust-native file system for PICKLE OS.
//!
//! NextFS is a classic Unix-style file system with inodes, directories, and data
//! blocks, but designed from scratch in Rust without the legacy baggage of ext2/3/4.
//! It's tailored for PICKLE OS's capability model and sits atop the [`crate::driver::block`]
//! abstraction layer.
//!
//! ## On-disk layout
//! ```text
//! Block 0:        Superblock (metadata, magic, pointers)
//! Block 1..N:     Free block bitmap (1 bit per block)
//! Block N+1..M:   Inode table (fixed-size inode entries)
//! Block M+1..:    Data blocks (file contents, indirect blocks)
//! ```
//!
//! ## Inode structure
//! - Type: file or directory (1 byte)
//! - Size: file size in bytes (8 bytes)
//! - Direct pointers: 12 × 4-byte block numbers (48 bytes)
//! - Single indirect pointer: 1 × 4-byte block number (4 bytes)
//! - Reserved/padding: bring to 64 bytes total for cache alignment
//!
//! ## Directory entries
//! Fixed 64-byte records: `[inode: u32 (4), name: [u8; 60]]`. A directory is a
//! file whose data blocks hold these records; inode 0 means unused entry.
//!
//! ## Design choices
//! - **Block size = 4096 bytes** (modern, matches page size).
//! - **Max file name = 59 chars + null terminator** (fits in 64-byte dir entry).
//! - **12 direct + 1 indirect** = up to 12 blocks direct (48 KiB), then 1024 more
//!   via the indirect block (4 MiB), so max file ~4 MiB without double-indirect.
//! - **Root directory = inode 1** (inode 0 is reserved/invalid).
//! - **Bitmap allocator**: simple first-fit for blocks and inodes.
//!
//! ## Future enhancements
//! - Double/triple indirect for larger files.
//! - Timestamps, permissions, hard links.
//! - Journaling or copy-on-write for crash consistency.
//! - Block groups (ext2-style) for better locality on large disks.

pub mod nextfs;
pub mod vfs;

pub use nextfs::{NextFS, FsError, FileStat, ROOT_INODE, Inode};
pub use vfs::{
    open, read, write, close, pipe, socket, seek, readdir, unlink, rmdir, mkdir, truncate,
    stat, chmod, chown, credentials, set_credentials, Credentials,
    init_task_fds, cleanup_task_fds, clone_task_fds, OpenFlags, SeekWhence, VfsError, Fd,
    STDIN, STDOUT, STDERR,
};

use spin::Mutex;

/// Global mounted filesystem (single mount point for now).
///
/// # Interrupt safety
///
/// This must NOT be accessed with a plain `.lock()` — the lock does not disable
/// interrupts. On a single-core preemptive kernel a `spin::Mutex` deadlocks if a
/// timer interrupt preempts the holder and the ISR or a new task tries to
/// re-enter the same lock. Use [`lock_fs`] / [`lock_fs_mut`] instead, which
/// disable interrupts for the duration of the critical section.
static MOUNTED_FS: Mutex<Option<NextFS>> = Mutex::new(None);

/// Lock the filesystem Mutex with interrupts disabled, and return a guard.
///
/// The guard automatically restores the interrupt flag when dropped, so
/// interrupts are re-enabled once the caller is done with the filesystem.
/// This is safe because `spin::MutexGuard` on single-core is not `Send`, so
/// the guard cannot leak across scheduler ticks even with the IRQ state
/// mediation that `without_interrupts` provides.
fn lock_fs() -> spin::MutexGuard<'static, Option<NextFS>> {
    x86_64::instructions::interrupts::without_interrupts(|| MOUNTED_FS.lock())
}

/// Mount a NextFS from the given block device index.
pub fn mount(dev_idx: usize) -> Result<(), FsError> {
    let fs = NextFS::mount(dev_idx)?;
    *lock_fs() = Some(fs);
    Ok(())
}

/// Unmount the current filesystem.
pub fn unmount() {
    *lock_fs() = None;
}

/// Non-blocking check for whether a filesystem is currently mounted. Uses
/// `try_lock` so a caller (e.g. the shell's lazy history load) is never blocked
/// while another task holds the lock for a long operation such as the
/// first-boot format/self-test. Returns `false` if the lock is contended.
///
/// This is inherently racy (the result is stale by the time the caller reads
/// it), but it is only used as a best-effort hint. It *does* disable interrupts
/// during the try_lock to avoid a preempted holding task causing a deadlock.
pub fn is_mounted() -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| {
        MOUNTED_FS
            .try_lock()
            .map(|g| g.is_some())
            .unwrap_or(false)
    })
}

/// Execute a closure with a reference to the mounted filesystem.
/// Interrupts are disabled for the entire duration.
pub fn with_fs<F, R>(f: F) -> R
where
    F: FnOnce(Option<&NextFS>) -> R,
{
    let fs_guard = lock_fs();
    f(fs_guard.as_ref())
}

/// Execute a closure with a mutable reference to the mounted filesystem.
/// Interrupts are disabled for the entire duration.
pub fn with_fs_mut<F, R>(f: F) -> R
where
    F: FnOnce(Option<&mut NextFS>) -> R,
{
    let mut fs_guard = lock_fs();
    f(fs_guard.as_mut())
}
