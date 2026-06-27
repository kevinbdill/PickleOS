//! Virtual File System (VFS) shim — provides syscall-level file operations.

use super::nextfs::{
    FileStat, FsError, NextFS, MAY_READ, MAY_WRITE, ROOT_INODE,
};
use super::{with_fs, with_fs_mut};
use crate::serial_println;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

/// Maximum number of open files per task.
const MAX_FDS_PER_TASK: usize = 64;

/// File descriptor number.
pub type Fd = u32;

/// Standard file descriptors.
pub const STDIN: Fd = 0;
pub const STDOUT: Fd = 1;
pub const STDERR: Fd = 2;

/// Open file flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenFlags {
    pub read: bool,
    pub write: bool,
    pub create: bool,
    pub truncate: bool,
    pub append: bool,
}

impl OpenFlags {
    pub fn rdonly() -> Self {
        OpenFlags {
            read: true,
            write: false,
            create: false,
            truncate: false,
            append: false,
        }
    }

    pub fn wronly() -> Self {
        OpenFlags {
            read: false,
            write: true,
            create: false,
            truncate: false,
            append: false,
        }
    }

    pub fn rdwr() -> Self {
        OpenFlags {
            read: true,
            write: true,
            create: false,
            truncate: false,
            append: false,
        }
    }

    pub fn create() -> Self {
        OpenFlags {
            read: false,
            write: true,
            create: true,
            truncate: false,
            append: false,
        }
    }

    pub fn with_truncate(mut self) -> Self {
        self.truncate = true;
        self
    }

    pub fn with_append(mut self) -> Self {
        self.append = true;
        self
    }
}

/// Seek origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekWhence {
    Set,     // Absolute position
    Current, // Relative to current position
    End,     // Relative to end of file
}

/// Type of open file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileType {
    /// Regular file on NextFS.
    Regular(u32), // inode number
    /// Console output (VGA + serial).
    Console,
    /// Keyboard input.
    Keyboard,
    /// One end of an anonymous pipe. The `u64` is the pipe id in [`PIPES`]; the
    /// `bool` is `true` for the write end and `false` for the read end.
    Pipe(u64, bool),
    /// Network socket. The `u32` is the socket ID from the network stack.
    Socket(u32),
}

/// An open file handle.
#[derive(Debug, Clone)]
pub(crate) struct OpenFile {
    pub(crate) file_type: FileType,
    flags: OpenFlags,
    pos: u64,
}

impl OpenFile {
    fn new(inode: u32, flags: OpenFlags) -> Self {
        OpenFile {
            file_type: FileType::Regular(inode),
            flags,
            pos: 0,
        }
    }

    fn console(writable: bool) -> Self {
        OpenFile {
            file_type: FileType::Console,
            flags: if writable {
                OpenFlags::wronly()
            } else {
                OpenFlags::rdonly()
            },
            pos: 0,
        }
    }

    fn keyboard() -> Self {
        OpenFile {
            file_type: FileType::Keyboard,
            flags: OpenFlags::rdonly(),
            pos: 0,
        }
    }

    /// One end of a pipe: read end (`is_write == false`) is read-only, write
    /// end (`is_write == true`) is write-only.
    fn pipe(id: u64, is_write: bool) -> Self {
        OpenFile {
            file_type: FileType::Pipe(id, is_write),
            flags: if is_write {
                OpenFlags::wronly()
            } else {
                OpenFlags::rdonly()
            },
            pos: 0,
        }
    }
}

// ===========================================================================
// Anonymous pipes — a simple in-kernel IPC primitive.
//
// A pipe is a single fixed-capacity circular byte buffer with two ends: a read
// end and a write end. Multiple processes may hold each end (reference counted)
// because `fork` duplicates file descriptors. Semantics:
//   * read on an empty pipe blocks (cooperatively yields) until data arrives,
//     or returns 0 (EOF) once *all* write ends are closed.
//   * write on a full pipe blocks until space frees up; writing with no
//     remaining read ends fails (broken pipe).
// ===========================================================================

/// Capacity of a single pipe's circular buffer (one page).
const PIPE_CAPACITY: usize = 4096;

/// Monotonic pipe-id allocator.
static NEXT_PIPE_ID: AtomicU64 = AtomicU64::new(1);

/// Kernel-side state for one anonymous pipe.
struct Pipe {
    /// Circular byte buffer (front = next byte to read, back = last written).
    buf: VecDeque<u8>,
    /// Number of open read ends (across all tasks).
    read_ends: u32,
    /// Number of open write ends (across all tasks).
    write_ends: u32,
}

/// Global table of live pipes, keyed by pipe id.
static PIPES: Mutex<BTreeMap<u64, Pipe>> = Mutex::new(BTreeMap::new());

/// Increment the reference count for one end of a pipe (used by `fork`).
fn pipe_dup_end(id: u64, is_write: bool) {
    let mut pipes = PIPES.lock();
    if let Some(p) = pipes.get_mut(&id) {
        if is_write {
            p.write_ends += 1;
        } else {
            p.read_ends += 1;
        }
    }
}

/// Decrement the reference count for one end of a pipe, freeing the pipe once
/// both ends reach zero. Closing the last write end unblocks readers (EOF);
/// closing the last read end unblocks writers (broken pipe).
fn pipe_close_end(id: u64, is_write: bool) {
    let mut pipes = PIPES.lock();
    if let Some(p) = pipes.get_mut(&id) {
        if is_write {
            p.write_ends = p.write_ends.saturating_sub(1);
        } else {
            p.read_ends = p.read_ends.saturating_sub(1);
        }
        if p.read_ends == 0 && p.write_ends == 0 {
            pipes.remove(&id);
        }
    }
}

/// Per-task file descriptor table.
#[derive(Debug)]
pub(crate) struct FdTable {
    pub(crate) files: BTreeMap<Fd, OpenFile>,
    next_fd: Fd,
}

impl FdTable {
    fn new() -> Self {
        FdTable {
            files: BTreeMap::new(),
            next_fd: 3, // Skip stdin/stdout/stderr
        }
    }

    fn alloc_fd(&mut self, file: OpenFile) -> Result<Fd, VfsError> {
        if self.files.len() >= MAX_FDS_PER_TASK {
            return Err(VfsError::TooManyFiles);
        }

        // Find next available fd.
        while self.files.contains_key(&self.next_fd) {
            self.next_fd += 1;
            if self.next_fd >= MAX_FDS_PER_TASK as u32 {
                self.next_fd = 3;
            }
        }

        let fd = self.next_fd;
        self.files.insert(fd, file);
        self.next_fd += 1;
        Ok(fd)
    }

    fn get(&self, fd: Fd) -> Result<&OpenFile, VfsError> {
        self.files.get(&fd).ok_or(VfsError::BadFileDescriptor)
    }

    fn get_mut(&mut self, fd: Fd) -> Result<&mut OpenFile, VfsError> {
        self.files.get_mut(&fd).ok_or(VfsError::BadFileDescriptor)
    }

    fn close(&mut self, fd: Fd) -> Result<(), VfsError> {
        self.files.remove(&fd).ok_or(VfsError::BadFileDescriptor)?;
        Ok(())
    }

    fn close_all(&mut self) {
        self.files.clear();
    }
}

/// Global per-task file descriptor tables.
pub(crate) static FD_TABLES: Mutex<BTreeMap<u32, FdTable>> = Mutex::new(BTreeMap::new());

/// A task's security credentials (user/group id).
#[derive(Debug, Clone, Copy)]
pub struct Credentials {
    pub uid: u16,
    pub gid: u16,
}

impl Credentials {
    /// The superuser (root) credentials — bypasses permission checks.
    pub const fn root() -> Self {
        Credentials { uid: 0, gid: 0 }
    }
}

/// Global per-task credentials. A task with no entry is treated as root.
static CREDENTIALS: Mutex<BTreeMap<u32, Credentials>> = Mutex::new(BTreeMap::new());

/// Return the credentials for a task (defaults to root if unset).
pub fn credentials(task_id: u32) -> Credentials {
    CREDENTIALS
        .lock()
        .get(&task_id)
        .copied()
        .unwrap_or_else(Credentials::root)
}

/// Set the credentials (uid/gid) for a task.
pub fn set_credentials(task_id: u32, uid: u16, gid: u16) {
    CREDENTIALS.lock().insert(task_id, Credentials { uid, gid });
}

/// VFS errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsError {
    NotFound,
    NotFile,
    NotDirectory,
    AlreadyExists,
    NoSpace,
    BadFileDescriptor,
    InvalidOperation,
    ReadOnlyFile,
    WriteOnlyFile,
    TooManyFiles,
    NotMounted,
    PermissionDenied,
    FsError(FsError),
}

impl From<FsError> for VfsError {
    fn from(e: FsError) -> Self {
        match e {
            FsError::NotFound => VfsError::NotFound,
            FsError::NotDirectory => VfsError::NotDirectory,
            FsError::NotFile => VfsError::NotFile,
            FsError::AlreadyExists => VfsError::AlreadyExists,
            FsError::NoSpace => VfsError::NoSpace,
            FsError::PermissionDenied => VfsError::PermissionDenied,
            other => VfsError::FsError(other),
        }
    }
}

/// Initialize file descriptor table for a task.
pub fn init_task_fds(task_id: u32) {
    let mut tables = FD_TABLES.lock();
    let mut table = FdTable::new();
    
    // Pre-populate standard streams (stdin/stdout/stderr).
    // stdin (fd 0): keyboard input
    table.files.insert(STDIN, OpenFile::keyboard());
    // stdout (fd 1): console output
    table.files.insert(STDOUT, OpenFile::console(true));
    // stderr (fd 2): console output (same as stdout for now)
    table.files.insert(STDERR, OpenFile::console(true));
    
    tables.insert(task_id, table);
}

/// Clean up file descriptor table for a task.
pub fn cleanup_task_fds(task_id: u32) {
    // Remove the table first (releasing the FD_TABLES lock) so we never hold
    // both FD_TABLES and PIPES at once.
    let removed = {
        let mut tables = FD_TABLES.lock();
        tables.remove(&task_id)
    };
    if let Some(table) = removed {
        // Release any pipe ends this task held so reference counts stay correct
        // (e.g. so the last writer closing produces EOF for readers).
        for file in table.files.values() {
            if let FileType::Pipe(id, is_write) = file.file_type {
                pipe_close_end(id, is_write);
            }
            // Release any TCP sockets the task still had open.
            if let FileType::Socket(sock_id) = file.file_type {
                crate::net::stack::tcp_destroy(sock_id);
            }
        }
    }
    // Also drop any credentials this task held.
    CREDENTIALS.lock().remove(&task_id);
}

/// Duplicate `parent`'s entire file-descriptor table into `child`.
///
/// Used by `fork(2)`: the child inherits copies of every open file handle
/// (including the standard streams), each with its own independent file
/// position semantics going forward. Any pre-existing table for `child` is
/// replaced. The parent's credentials are copied too so the child runs with
/// the same uid/gid.
pub fn clone_task_fds(parent: u32, child: u32) {
    // Snapshot the parent's open files (OpenFile is Clone).
    let cloned: Option<(BTreeMap<Fd, OpenFile>, Fd)> = {
        let tables = FD_TABLES.lock();
        tables.get(&parent).map(|t| (t.files.clone(), t.next_fd))
    };

    let mut tables = FD_TABLES.lock();
    match cloned {
        Some((files, next_fd)) => {
            // The child now holds its own copy of every pipe end the parent had,
            // so bump each pipe's reference counts accordingly.
            for file in files.values() {
                if let FileType::Pipe(id, is_write) = file.file_type {
                    pipe_dup_end(id, is_write);
                }
            }
            tables.insert(child, FdTable { files, next_fd });
        }
        None => {
            // Parent had no table (shouldn't happen): give the child the
            // standard streams so it is still usable.
            let mut table = FdTable::new();
            table.files.insert(STDIN, OpenFile::keyboard());
            table.files.insert(STDOUT, OpenFile::console(true));
            table.files.insert(STDERR, OpenFile::console(true));
            tables.insert(child, table);
        }
    }

    // Inherit credentials (default root if the parent had none recorded).
    let creds = CREDENTIALS
        .lock()
        .get(&parent)
        .copied()
        .unwrap_or_else(Credentials::root);
    CREDENTIALS.lock().insert(child, creds);
}

/// Helper to split a path into (parent_path, filename).
fn split_path(path: &str) -> (&str, &str) {
    if let Some(pos) = path.rfind('/') {
        let parent = if pos == 0 { "/" } else { &path[..pos] };
        let name = &path[pos + 1..];
        (parent, name)
    } else {
        ("/", path)
    }
}

/// Resolve a path to an inode number.
fn resolve_path(path: &str) -> Result<u32, VfsError> {
    if path.is_empty() {
        return Err(VfsError::NotFound);
    }

    with_fs(|fs| {
        let fs = fs.ok_or(VfsError::NotMounted)?;

        if path == "/" {
            return Ok(ROOT_INODE);
        }

        let path = path.trim_start_matches('/');
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        let mut current_inode = ROOT_INODE;
        for part in parts {
            current_inode = fs.dir_lookup(current_inode, part)?;
        }

        Ok(current_inode)
    })
}

/// Open a file. Returns a file descriptor.
pub fn open(task_id: u32, path: &str, flags: OpenFlags) -> Result<Fd, VfsError> {
    let inode = if flags.create {
        // Try to open existing file, or create it.
        match resolve_path(path) {
            Ok(inode) => {
                // File exists.
                if flags.truncate {
                    with_fs_mut(|fs| {
                        let fs = fs.ok_or(VfsError::NotMounted)?;
                        fs.truncate(inode, 0)?;
                        Ok::<_, VfsError>(())
                    })?;
                }
                inode
            }
            Err(VfsError::NotFound) | Err(VfsError::FsError(FsError::NotFound)) => {
                // Create new file. The creating task must be able to write to
                // the parent directory, and becomes the owner of the new file.
                let (parent_path, name) = split_path(path);
                let parent_inode = resolve_path(parent_path)?;
                require_dir_write(task_id, parent_inode)?;

                let creds = credentials(task_id);
                with_fs_mut(|fs| {
                    let fs = fs.ok_or(VfsError::NotMounted)?;
                    let new_inode = fs.create_file(parent_inode, name)?;
                    // New file is owned by the creating task.
                    let _ = fs.chown(new_inode, creds.uid, creds.gid);
                    Ok::<_, VfsError>(new_inode)
                })?
            }
            Err(e) => return Err(e),
        }
    } else {
        // Open existing file.
        resolve_path(path)?
    };

    // Enforce permissions: the opening task must have read and/or write
    // access to the inode according to the requested flags.
    let creds = credentials(task_id);
    let mut want = 0u16;
    if flags.read {
        want |= MAY_READ;
    }
    if flags.write {
        want |= MAY_WRITE;
    }
    if want != 0 {
        with_fs(|fs| {
            let fs = fs.ok_or(VfsError::NotMounted)?;
            fs.check_permission(inode, creds.uid, creds.gid, want)
                .map_err(VfsError::from)
        })?;
    }

    let mut pos = 0;
    if flags.append {
        // Seek to end of file.
        pos = with_fs(|fs| {
            let fs = fs.ok_or(VfsError::NotMounted)?;
            let inode_obj = fs.read_inode(inode)?;
            Ok::<_, VfsError>(inode_obj.size)
        })?;
    }

    let mut file = OpenFile::new(inode, flags);
    file.pos = pos;

    let mut tables = FD_TABLES.lock();
    let table = tables.get_mut(&task_id).ok_or(VfsError::InvalidOperation)?;
    table.alloc_fd(file)
}

/// Read from a file descriptor.
pub fn read(task_id: u32, fd: Fd, buf: &mut [u8]) -> Result<usize, VfsError> {
    // Get the file type and current position.
    let (file_type, pos) = {
        let tables = FD_TABLES.lock();
        let table = tables.get(&task_id).ok_or(VfsError::InvalidOperation)?;
        let file = table.get(fd)?;

        if !file.flags.read {
            return Err(VfsError::WriteOnlyFile);
        }

        (file.file_type, file.pos)
    };

    // Perform the read based on file type.
    let bytes_read = match file_type {
        FileType::Regular(inode) => {
            // Read from NextFS.
            with_fs(|fs| {
                let fs = fs.ok_or(VfsError::NotMounted)?;
                fs.read_inode_data(inode, pos, buf).map_err(VfsError::from)
            })?
        }
        FileType::Keyboard => {
            // Read from stdin with proper blocking semantics.
            // Block until at least one character is available, then drain the
            // buffer to fill as much of the user's request as possible.
            let mut n = 0;
            
            // First character: block until available.
            if buf.len() > 0 {
                let c = crate::driver::console::read_char_blocking();
                let mut tmp = [0u8; 4];
                let encoded = c.encode_utf8(&mut tmp).as_bytes();
                if encoded.len() <= buf.len() {
                    buf[n..n + encoded.len()].copy_from_slice(encoded);
                    n += encoded.len();
                }
            }
            
            // Read any additional buffered characters non-blocking.
            while n < buf.len() {
                match crate::driver::console::read_char() {
                    Some(c) => {
                        let mut tmp = [0u8; 4];
                        let encoded = c.encode_utf8(&mut tmp).as_bytes();
                        if n + encoded.len() > buf.len() {
                            break;
                        }
                        buf[n..n + encoded.len()].copy_from_slice(encoded);
                        n += encoded.len();
                    }
                    None => break,
                }
            }
            n
        }
        FileType::Console => {
            // Console is write-only.
            return Err(VfsError::WriteOnlyFile);
        }
        FileType::Pipe(id, is_write) => {
            // Reading the write end is invalid.
            if is_write {
                return Err(VfsError::WriteOnlyFile);
            }
            // Block (cooperatively yield) until bytes are available, or return
            // 0 (EOF) once every write end has been closed.
            loop {
                let result = {
                    let mut pipes = PIPES.lock();
                    match pipes.get_mut(&id) {
                        Some(p) => {
                            if !p.buf.is_empty() {
                                let mut n = 0;
                                while n < buf.len() {
                                    match p.buf.pop_front() {
                                        Some(b) => {
                                            buf[n] = b;
                                            n += 1;
                                        }
                                        None => break,
                                    }
                                }
                                Some(n) // delivered some data
                            } else if p.write_ends == 0 {
                                Some(0) // EOF: no data and no writers
                            } else {
                                None // empty but writers remain -> block
                            }
                        }
                        None => Some(0), // pipe gone -> EOF
                    }
                };
                match result {
                    Some(n) => break n,
                    None => crate::task::yield_now(),
                }
            }
        }
        FileType::Socket(socket_id) => {
            // Read from network socket - delegate to network stack.
            match crate::net::stack::tcp_recv(socket_id, buf) {
                Ok(n) => n,
                Err(_) => return Err(VfsError::InvalidOperation),
            }
        }
    };

    // Update position for regular files.
    if matches!(file_type, FileType::Regular(_)) {
        let mut tables = FD_TABLES.lock();
        let table = tables.get_mut(&task_id).ok_or(VfsError::InvalidOperation)?;
        let file = table.get_mut(fd)?;
        file.pos += bytes_read as u64;
    }

    Ok(bytes_read)
}

/// Write to a file descriptor.
pub fn write(task_id: u32, fd: Fd, buf: &[u8]) -> Result<usize, VfsError> {
    // Get the file type and current position.
    let (file_type, pos) = {
        let tables = FD_TABLES.lock();
        let table = tables.get(&task_id).ok_or(VfsError::InvalidOperation)?;
        let file = table.get(fd)?;

        if !file.flags.write {
            return Err(VfsError::ReadOnlyFile);
        }

        (file.file_type, file.pos)
    };

    // Perform the write based on file type.
    let bytes_written = match file_type {
        FileType::Regular(inode) => {
            // Write to NextFS.
            with_fs_mut(|fs| {
                let fs = fs.ok_or(VfsError::NotMounted)?;
                let mut inode_obj = fs.read_inode(inode)?;
                let written = fs.write_inode_data(&mut inode_obj, pos, buf)?;
                fs.write_inode(inode, &inode_obj)?;
                Ok::<_, VfsError>(written)
            })?
        }
        FileType::Console => {
            // Write to VGA console and serial port.
            use crate::{print, serial_print};
            for &byte in buf {
                // Print to VGA
                print!("{}", byte as char);
                // Also send to serial for logging
                serial_print!("{}", byte as char);
            }
            buf.len()
        }
        FileType::Keyboard => {
            // Keyboard is read-only.
            return Err(VfsError::ReadOnlyFile);
        }
        FileType::Pipe(id, is_write) => {
            // Writing the read end is invalid.
            if !is_write {
                return Err(VfsError::ReadOnlyFile);
            }
            // Push all bytes into the circular buffer, blocking (yielding) when
            // it is full. If no read ends remain, the pipe is broken.
            let mut written = 0usize;
            loop {
                let mut full = false;
                {
                    let mut pipes = PIPES.lock();
                    let p = match pipes.get_mut(&id) {
                        Some(p) => p,
                        None => break, // pipe gone
                    };
                    if p.read_ends == 0 {
                        // Broken pipe: nobody can ever read what we write.
                        break;
                    }
                    let space = PIPE_CAPACITY - p.buf.len();
                    if space == 0 {
                        full = true;
                    } else {
                        let take = core::cmp::min(space, buf.len() - written);
                        for &b in &buf[written..written + take] {
                            p.buf.push_back(b);
                        }
                        written += take;
                    }
                }
                if written >= buf.len() {
                    break;
                }
                if full {
                    crate::task::yield_now();
                }
            }
            if written == 0 && !buf.is_empty() {
                // Could not write anything (broken pipe with no readers).
                return Err(VfsError::InvalidOperation);
            }
            written
        }
        FileType::Socket(socket_id) => {
            // Write to network socket - delegate to network stack.
            match crate::net::stack::tcp_send(socket_id, buf) {
                Ok(n) => n,
                Err(_) => return Err(VfsError::InvalidOperation),
            }
        }
    };

    // Update position for regular files.
    if matches!(file_type, FileType::Regular(_)) {
        let mut tables = FD_TABLES.lock();
        let table = tables.get_mut(&task_id).ok_or(VfsError::InvalidOperation)?;
        let file = table.get_mut(fd)?;
        file.pos += bytes_written as u64;
    }

    Ok(bytes_written)
}

/// Seek within a file.
pub fn seek(task_id: u32, fd: Fd, offset: i64, whence: SeekWhence) -> Result<u64, VfsError> {
    let mut tables = FD_TABLES.lock();
    let table = tables.get_mut(&task_id).ok_or(VfsError::InvalidOperation)?;
    let file = table.get_mut(fd)?;

    // Only regular files support seeking.
    let inode = match file.file_type {
        FileType::Regular(ino) => ino,
        FileType::Console | FileType::Keyboard | FileType::Pipe(..) | FileType::Socket(_) => {
            return Err(VfsError::InvalidOperation);
        }
    };

    // Get file size.
    let file_size = with_fs(|fs| {
        let fs = fs.ok_or(VfsError::NotMounted)?;
        let inode_obj = fs.read_inode(inode)?;
        Ok::<_, VfsError>(inode_obj.size)
    })?;

    let new_pos = match whence {
        SeekWhence::Set => {
            if offset < 0 {
                return Err(VfsError::InvalidOperation);
            }
            offset as u64
        }
        SeekWhence::Current => {
            let result = (file.pos as i64) + offset;
            if result < 0 {
                return Err(VfsError::InvalidOperation);
            }
            result as u64
        }
        SeekWhence::End => {
            let result = (file_size as i64) + offset;
            if result < 0 {
                return Err(VfsError::InvalidOperation);
            }
            result as u64
        }
    };

    file.pos = new_pos;
    Ok(new_pos)
}

/// Close a file descriptor.
pub fn close(task_id: u32, fd: Fd) -> Result<(), VfsError> {
    // Determine whether this fd is a pipe end or socket (so we can drop its reference
    // afterwards), then remove it from the table — releasing FD_TABLES before
    // touching PIPES/network stack to keep a consistent lock order.
    let (pipe_end, socket_id) = {
        let mut tables = FD_TABLES.lock();
        let table = tables.get_mut(&task_id).ok_or(VfsError::InvalidOperation)?;
        let (pipe, sock) = match table.files.get(&fd) {
            Some(f) => {
                let pipe = if let FileType::Pipe(id, is_write) = f.file_type {
                    Some((id, is_write))
                } else {
                    None
                };
                let sock = if let FileType::Socket(id) = f.file_type {
                    Some(id)
                } else {
                    None
                };
                (pipe, sock)
            }
            None => return Err(VfsError::BadFileDescriptor),
        };
        table.close(fd)?;
        (pipe, sock)
    };
    
    if let Some((id, is_write)) = pipe_end {
        pipe_close_end(id, is_write);
    }
    if let Some(sock_id) = socket_id {
        // Fully release the underlying smoltcp socket (close + free buffers).
        crate::net::stack::tcp_destroy(sock_id);
    }
    Ok(())
}

/// Create an anonymous pipe owned by `task_id`. Returns `(read_fd, write_fd)`.
pub fn pipe(task_id: u32) -> Result<(Fd, Fd), VfsError> {
    // Acquire FD_TABLES first, then PIPES (consistent with close() and
    // cleanup_task_fds — never reverse this order to avoid deadlock).
    let mut tables = FD_TABLES.lock();
    let table = tables.get_mut(&task_id).ok_or(VfsError::InvalidOperation)?;

    // Allocate the pipe object. We hold FD_TABLES, not PIPES, so we stash
    // the pipe metadata and insert into PIPES afterwards.
    let id = NEXT_PIPE_ID.fetch_add(1, Ordering::SeqCst);

    let read_fd = table.alloc_fd(OpenFile::pipe(id, false))?;
    let write_fd = table.alloc_fd(OpenFile::pipe(id, true))?;

    // Now register the pipe. FD_TABLES is still held, which is fine because
    // the pipe metadata is tiny and PIPES is an independent lock.
    drop(tables);
    PIPES.lock().insert(
        id,
        Pipe {
            buf: VecDeque::new(),
            read_ends: 1,
            write_ends: 1,
        },
    );

    Ok((read_fd, write_fd))
}

/// Create a socket file descriptor for the given task. Returns the socket FD.
pub fn socket(task_id: u32, socket_id: u32) -> Result<Fd, VfsError> {
    let mut tables = FD_TABLES.lock();
    let table = match tables.get_mut(&task_id) {
        Some(t) => t,
        None => return Err(VfsError::InvalidOperation),
    };
    
    let mut flags = OpenFlags { read: false, write: false, create: false, truncate: false, append: false };
    flags.read = true;
    flags.write = true;
    
    table.alloc_fd(OpenFile {
        file_type: FileType::Socket(socket_id),
        pos: 0,
        flags,
    })
}

/// Read directory entries.
pub fn readdir(task_id: u32, path: &str) -> Result<Vec<(String, u32)>, VfsError> {
    let inode = resolve_path(path)?;

    with_fs(|fs| {
        let fs = fs.ok_or(VfsError::NotMounted)?;
        let entries = fs.dir_list(inode)?;
        Ok(entries)
    })
}

/// Require that `task_id` has write permission on a directory inode.
fn require_dir_write(task_id: u32, dir_inode: u32) -> Result<(), VfsError> {
    let creds = credentials(task_id);
    with_fs(|fs| {
        let fs = fs.ok_or(VfsError::NotMounted)?;
        fs.check_permission(dir_inode, creds.uid, creds.gid, MAY_WRITE)
            .map_err(VfsError::from)
    })
}

/// Unlink (delete) a file.
pub fn unlink(task_id: u32, path: &str) -> Result<(), VfsError> {
    let (parent_path, name) = split_path(path);
    let parent_inode = resolve_path(parent_path)?;
    require_dir_write(task_id, parent_inode)?;

    with_fs_mut(|fs| {
        let fs = fs.ok_or(VfsError::NotMounted)?;
        fs.unlink(parent_inode, name)?;
        Ok(())
    })
}

/// Remove a directory.
pub fn rmdir(task_id: u32, path: &str) -> Result<(), VfsError> {
    let (parent_path, name) = split_path(path);
    let parent_inode = resolve_path(parent_path)?;
    require_dir_write(task_id, parent_inode)?;

    with_fs_mut(|fs| {
        let fs = fs.ok_or(VfsError::NotMounted)?;
        fs.rmdir(parent_inode, name)?;
        Ok(())
    })
}

/// Create a directory. The new directory inherits the creating task's
/// uid/gid as owner.
pub fn mkdir(task_id: u32, path: &str) -> Result<(), VfsError> {
    let (parent_path, name) = split_path(path);
    let parent_inode = resolve_path(parent_path)?;
    require_dir_write(task_id, parent_inode)?;

    let creds = credentials(task_id);
    with_fs_mut(|fs| {
        let fs = fs.ok_or(VfsError::NotMounted)?;
        let new_inode = fs.create_dir(parent_inode, name)?;
        // Assign ownership to the creating task (mode keeps the FS default).
        let _ = fs.chown(new_inode, creds.uid, creds.gid);
        Ok(())
    })
}

/// Truncate a file to a specific size. Requires write permission.
pub fn truncate(task_id: u32, path: &str, size: u64) -> Result<(), VfsError> {
    let inode = resolve_path(path)?;
    let creds = credentials(task_id);

    with_fs_mut(|fs| {
        let fs = fs.ok_or(VfsError::NotMounted)?;
        fs.check_permission(inode, creds.uid, creds.gid, MAY_WRITE)?;
        fs.truncate(inode, size)?;
        Ok(())
    })
}

/// Return metadata (`stat`) for a path.
pub fn stat(_task_id: u32, path: &str) -> Result<FileStat, VfsError> {
    let inode = resolve_path(path)?;
    with_fs(|fs| {
        let fs = fs.ok_or(VfsError::NotMounted)?;
        let st = fs.stat(inode)?;
        Ok(st)
    })
}

/// Change permission bits of a path. Only the owner or root may chmod.
pub fn chmod(task_id: u32, path: &str, mode: u16) -> Result<(), VfsError> {
    let inode = resolve_path(path)?;
    let creds = credentials(task_id);

    with_fs_mut(|fs| {
        let fs = fs.ok_or(VfsError::NotMounted)?;
        let st = fs.stat(inode)?;
        if creds.uid != 0 && creds.uid != st.uid {
            return Err(VfsError::PermissionDenied);
        }
        fs.chmod(inode, mode)?;
        Ok(())
    })
}

/// Change owner/group of a path. Only root may chown.
pub fn chown(task_id: u32, path: &str, uid: u16, gid: u16) -> Result<(), VfsError> {
    let inode = resolve_path(path)?;
    let creds = credentials(task_id);

    if creds.uid != 0 {
        return Err(VfsError::PermissionDenied);
    }

    with_fs_mut(|fs| {
        let fs = fs.ok_or(VfsError::NotMounted)?;
        fs.chown(inode, uid, gid)?;
        Ok(())
    })
}
