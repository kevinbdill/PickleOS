//! VFS server: a filesystem service reachable over IPC.
//!
//! The VFS server owns a named IPC endpoint (`"vfs"`). Clients build a
//! [`VfsRequest`], box it on the heap, and hand the pointer to the server via an
//! IPC `call`. The server takes ownership of the request, dispatches it to the
//! MemFS backend, boxes a [`VfsResponse`], and replies with that pointer. The
//! client then reclaims the response box.
//!
//! Passing heap pointers through the 6 inline message words is sound here
//! because every server in this phase shares the kernel address space, and the
//! synchronous `call`/`reply` discipline guarantees the request box outlives the
//! server's read (the caller stays blocked until the reply arrives). When these
//! services migrate to isolated user address spaces, this marshalling layer is
//! the single place that will switch to shared-memory or by-value copying.

use super::memfs::{self, FsError};
use crate::ipc::{self, Message};
use crate::serial_println;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// Well-known name the VFS server registers under.
pub const VFS_ENDPOINT: &str = "vfs";

/// Message tag identifying a VFS RPC.
const TAG_VFS: u64 = 0x5646_5300; // "VFS\0"

/// A request sent from a client to the VFS server.
pub enum VfsRequest {
    Create { path: String },
    Mkdir { path: String },
    Write { path: String, data: Vec<u8> },
    Read { path: String },
    ReadDir { path: String },
    Stat { path: String },
    Remove { path: String },
}

/// A response sent from the VFS server back to a client.
pub enum VfsResponse {
    Ok,
    Data(Vec<u8>),
    Entries(Vec<String>),
    Stat { size: usize, is_dir: bool },
    Written(usize),
    Error(FsError),
}

/// Dispatch a single request against the MemFS backend.
fn handle(req: VfsRequest) -> VfsResponse {
    match req {
        VfsRequest::Create { path } => match memfs::create(&path) {
            Ok(()) => VfsResponse::Ok,
            Err(e) => VfsResponse::Error(e),
        },
        VfsRequest::Mkdir { path } => match memfs::mkdir(&path) {
            Ok(()) => VfsResponse::Ok,
            Err(e) => VfsResponse::Error(e),
        },
        VfsRequest::Write { path, data } => match memfs::write(&path, &data) {
            Ok(n) => VfsResponse::Written(n),
            Err(e) => VfsResponse::Error(e),
        },
        VfsRequest::Read { path } => match memfs::read(&path) {
            Ok(data) => VfsResponse::Data(data),
            Err(e) => VfsResponse::Error(e),
        },
        VfsRequest::ReadDir { path } => match memfs::readdir(&path) {
            Ok(entries) => VfsResponse::Entries(entries),
            Err(e) => VfsResponse::Error(e),
        },
        VfsRequest::Stat { path } => match memfs::stat(&path) {
            Ok(info) => VfsResponse::Stat {
                size: info.size,
                is_dir: info.is_dir,
            },
            Err(e) => VfsResponse::Error(e),
        },
        VfsRequest::Remove { path } => match memfs::remove(&path) {
            Ok(()) => VfsResponse::Ok,
            Err(e) => VfsResponse::Error(e),
        },
    }
}

/// The VFS server task entry point. Registers the endpoint, seeds the backend,
/// then services requests forever.
pub extern "C" fn vfs_server_task() -> ! {
    memfs::init();
    let ep = ipc::create_named_endpoint(VFS_ENDPOINT);
    serial_println!("[vfs] server online (endpoint {})", ep);

    loop {
        let msg = ipc::receive(ep);
        if msg.tag != TAG_VFS {
            serial_println!("[vfs] dropping message with unexpected tag {:#x}", msg.tag);
            continue;
        }
        // Reclaim ownership of the request box the client handed us.
        let req_ptr = msg.words[0] as *mut VfsRequest;
        if req_ptr.is_null() {
            continue;
        }
        let req = unsafe { *Box::from_raw(req_ptr) };
        let resp = handle(req);
        let resp_ptr = Box::into_raw(Box::new(resp)) as u64;
        ipc::reply(&msg, Message::with_words(TAG_VFS, [resp_ptr, 0, 0, 0, 0, 0]));
    }
}

/// Client helper: perform a synchronous VFS RPC. Returns `None` if the VFS
/// server is not yet registered.
pub fn request(req: VfsRequest) -> Option<VfsResponse> {
    let ep = ipc::lookup(VFS_ENDPOINT)?;
    let req_ptr = Box::into_raw(Box::new(req)) as u64;
    let reply = ipc::call(ep, Message::with_words(TAG_VFS, [req_ptr, 0, 0, 0, 0, 0])).unwrap_or_else(|_| Message::default());
    let resp_ptr = reply.words[0] as *mut VfsResponse;
    if resp_ptr.is_null() {
        return None;
    }
    let resp = unsafe { *Box::from_raw(resp_ptr) };
    Some(resp)
}

// --- Convenience wrappers used by the shell and other clients --------------

/// Read a file's contents over IPC.
pub fn read(path: &str) -> Result<Vec<u8>, FsError> {
    match request(VfsRequest::Read {
        path: String::from(path),
    }) {
        Some(VfsResponse::Data(d)) => Ok(d),
        Some(VfsResponse::Error(e)) => Err(e),
        _ => Err(FsError::InvalidPath),
    }
}

/// Write (create/truncate) a file over IPC.
pub fn write(path: &str, data: &[u8]) -> Result<usize, FsError> {
    match request(VfsRequest::Write {
        path: String::from(path),
        data: data.to_vec(),
    }) {
        Some(VfsResponse::Written(n)) => Ok(n),
        Some(VfsResponse::Error(e)) => Err(e),
        _ => Err(FsError::InvalidPath),
    }
}

/// List a directory over IPC.
pub fn readdir(path: &str) -> Result<Vec<String>, FsError> {
    match request(VfsRequest::ReadDir {
        path: String::from(path),
    }) {
        Some(VfsResponse::Entries(e)) => Ok(e),
        Some(VfsResponse::Error(e)) => Err(e),
        _ => Err(FsError::InvalidPath),
    }
}

/// Stat a path over IPC.
pub fn stat(path: &str) -> Result<memfs::StatInfo, FsError> {
    match request(VfsRequest::Stat {
        path: String::from(path),
    }) {
        Some(VfsResponse::Stat { size, is_dir }) => Ok(memfs::StatInfo { size, is_dir }),
        Some(VfsResponse::Error(e)) => Err(e),
        _ => Err(FsError::InvalidPath),
    }
}

/// Create an empty file over IPC.
pub fn create(path: &str) -> Result<(), FsError> {
    match request(VfsRequest::Create {
        path: String::from(path),
    }) {
        Some(VfsResponse::Ok) => Ok(()),
        Some(VfsResponse::Error(e)) => Err(e),
        _ => Err(FsError::InvalidPath),
    }
}

/// Create a directory over IPC.
pub fn mkdir(path: &str) -> Result<(), FsError> {
    match request(VfsRequest::Mkdir {
        path: String::from(path),
    }) {
        Some(VfsResponse::Ok) => Ok(()),
        Some(VfsResponse::Error(e)) => Err(e),
        _ => Err(FsError::InvalidPath),
    }
}

/// Remove a file or empty directory over IPC.
pub fn remove(path: &str) -> Result<(), FsError> {
    match request(VfsRequest::Remove {
        path: String::from(path),
    }) {
        Some(VfsResponse::Ok) => Ok(()),
        Some(VfsResponse::Error(e)) => Err(e),
        _ => Err(FsError::InvalidPath),
    }
}
