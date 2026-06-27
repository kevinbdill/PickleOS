//! MemFS: a tiny in-memory filesystem backend.
//!
//! MemFS stores a flat map of absolute paths (`/foo/bar`) to nodes. A node is
//! either a regular file (a byte vector) or a directory. Directory listings are
//! computed on demand by scanning for immediate children of a path. This is
//! deliberately simple — it exists to give the VFS server a real backend so the
//! end-to-end file path (shell -> VFS -> MemFS over IPC) can be demonstrated.
//!
//! MemFS is *not* a server itself; it is a passive library called by the VFS
//! server task. All access is serialized through a single spinlock.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use spin::Mutex;

/// A filesystem node.
enum Node {
    /// Regular file with its byte contents.
    File(Vec<u8>),
    /// Directory (children are tracked implicitly by path prefix).
    Dir,
}

/// Errors returned by MemFS operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    AlreadyExists,
    NotADirectory,
    IsADirectory,
    InvalidPath,
}

/// Result of a `stat` call.
#[derive(Debug, Clone, Copy)]
pub struct StatInfo {
    pub size: usize,
    pub is_dir: bool,
}

/// The global filesystem map, initialized by [`init`].
static FS: Mutex<Option<BTreeMap<String, Node>>> = Mutex::new(None);

/// Normalize a path: must be absolute, strip any trailing slash (except root).
fn normalize(path: &str) -> Result<String, FsError> {
    if !path.starts_with('/') {
        return Err(FsError::InvalidPath);
    }
    if path == "/" {
        return Ok("/".to_string());
    }
    let trimmed = path.trim_end_matches('/');
    Ok(trimmed.to_string())
}

/// Return the parent directory of `path` (e.g. `/a/b` -> `/a`, `/a` -> `/`).
fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => path[..i].to_string(),
        None => "/".to_string(),
    }
}

/// Initialize MemFS with a root directory and a couple of seed files so a
/// freshly booted system has something to list and read.
pub fn init() {
    let mut guard = FS.lock();
    let mut m = BTreeMap::new();
    m.insert("/".to_string(), Node::Dir);
    m.insert("/etc".to_string(), Node::Dir);
    m.insert(
        "/welcome.txt".to_string(),
        Node::File(b"Welcome to PICKLE OS Phase 2!\nFiles are served over IPC by the VFS server.\n".to_vec()),
    );
    m.insert(
        "/etc/motd".to_string(),
        Node::File(b"PICKLE OS :: capability-secured microkernel\n".to_vec()),
    );
    *guard = Some(m);
}

/// Create an empty regular file. Fails if the path already exists or the parent
/// directory does not exist.
pub fn create(path: &str) -> Result<(), FsError> {
    let path = normalize(path)?;
    let mut guard = FS.lock();
    let fs = guard.as_mut().ok_or(FsError::InvalidPath)?;
    if fs.contains_key(&path) {
        return Err(FsError::AlreadyExists);
    }
    let parent = parent_of(&path);
    match fs.get(&parent) {
        Some(Node::Dir) => {}
        Some(_) => return Err(FsError::NotADirectory),
        None => return Err(FsError::NotFound),
    }
    fs.insert(path, Node::File(Vec::new()));
    Ok(())
}

/// Create a directory.
pub fn mkdir(path: &str) -> Result<(), FsError> {
    let path = normalize(path)?;
    let mut guard = FS.lock();
    let fs = guard.as_mut().ok_or(FsError::InvalidPath)?;
    if fs.contains_key(&path) {
        return Err(FsError::AlreadyExists);
    }
    let parent = parent_of(&path);
    match fs.get(&parent) {
        Some(Node::Dir) => {}
        Some(_) => return Err(FsError::NotADirectory),
        None => return Err(FsError::NotFound),
    }
    fs.insert(path, Node::Dir);
    Ok(())
}

/// Write `data` to a file, creating it if it does not exist (truncating if it
/// does). Fails if the path is an existing directory.
pub fn write(path: &str, data: &[u8]) -> Result<usize, FsError> {
    let path = normalize(path)?;
    let mut guard = FS.lock();
    let fs = guard.as_mut().ok_or(FsError::InvalidPath)?;
    match fs.get(&path) {
        Some(Node::Dir) => return Err(FsError::IsADirectory),
        Some(Node::File(_)) | None => {}
    }
    if !fs.contains_key(&path) {
        // Ensure parent exists before creating.
        let parent = parent_of(&path);
        match fs.get(&parent) {
            Some(Node::Dir) => {}
            Some(_) => return Err(FsError::NotADirectory),
            None => return Err(FsError::NotFound),
        }
    }
    fs.insert(path, Node::File(data.to_vec()));
    Ok(data.len())
}

/// Read the full contents of a file.
pub fn read(path: &str) -> Result<Vec<u8>, FsError> {
    let path = normalize(path)?;
    let guard = FS.lock();
    let fs = guard.as_ref().ok_or(FsError::InvalidPath)?;
    match fs.get(&path) {
        Some(Node::File(data)) => Ok(data.clone()),
        Some(Node::Dir) => Err(FsError::IsADirectory),
        None => Err(FsError::NotFound),
    }
}

/// List the immediate children of a directory, returning their leaf names.
pub fn readdir(path: &str) -> Result<Vec<String>, FsError> {
    let path = normalize(path)?;
    let guard = FS.lock();
    let fs = guard.as_ref().ok_or(FsError::InvalidPath)?;
    match fs.get(&path) {
        Some(Node::Dir) => {}
        Some(_) => return Err(FsError::NotADirectory),
        None => return Err(FsError::NotFound),
    }
    let prefix = if path == "/" {
        "/".to_string()
    } else {
        alloc::format!("{}/", path)
    };
    let mut entries = Vec::new();
    for key in fs.keys() {
        if key == &path {
            continue;
        }
        if let Some(rest) = key.strip_prefix(&prefix) {
            if !rest.is_empty() && !rest.contains('/') {
                entries.push(rest.to_string());
            }
        }
    }
    entries.sort();
    Ok(entries)
}

/// Return metadata about a path.
pub fn stat(path: &str) -> Result<StatInfo, FsError> {
    let path = normalize(path)?;
    let guard = FS.lock();
    let fs = guard.as_ref().ok_or(FsError::InvalidPath)?;
    match fs.get(&path) {
        Some(Node::File(data)) => Ok(StatInfo {
            size: data.len(),
            is_dir: false,
        }),
        Some(Node::Dir) => Ok(StatInfo {
            size: 0,
            is_dir: true,
        }),
        None => Err(FsError::NotFound),
    }
}

/// Remove a file or an empty directory.
pub fn remove(path: &str) -> Result<(), FsError> {
    let path = normalize(path)?;
    if path == "/" {
        return Err(FsError::InvalidPath);
    }
    let mut guard = FS.lock();
    let fs = guard.as_mut().ok_or(FsError::InvalidPath)?;
    if !fs.contains_key(&path) {
        return Err(FsError::NotFound);
    }
    // Refuse to remove a non-empty directory.
    if let Some(Node::Dir) = fs.get(&path) {
        let prefix = alloc::format!("{}/", path);
        if fs.keys().any(|k| k.starts_with(&prefix)) {
            return Err(FsError::NotADirectory);
        }
    }
    fs.remove(&path);
    Ok(())
}
