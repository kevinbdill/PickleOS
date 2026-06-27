//! Registry server: a name -> endpoint directory reachable over IPC.
//!
//! While the IPC layer already supports well-known endpoint names, the Registry
//! is a first-class *service* that owns the namespace policy: services register
//! themselves at runtime and clients discover them by name. This mirrors the
//! "naming service" role in classic microkernels (e.g. the seL4/Genode root
//! servers) and gives us a single place to later add access control on who may
//! register or resolve a given name.
//!
//! The marshalling approach matches the VFS server: boxed request/response
//! pointers are passed through the inline message words.

use crate::ipc::{self, Message};
use crate::serial_println;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use spin::Mutex;

/// Well-known name the Registry registers under.
pub const REGISTRY_ENDPOINT: &str = "registry";

/// Message tag identifying a Registry RPC.
const TAG_REG: u64 = 0x5245_4700; // "REG\0"

/// The registry's name table (name -> endpoint id). Stored alongside the server
/// task; access is serialized through this lock.
static NAMES: Mutex<Option<BTreeMap<String, u64>>> = Mutex::new(None);

/// Request from a client to the Registry.
pub enum RegRequest {
    Register { name: String, endpoint: u64 },
    Lookup { name: String },
    List,
}

/// Response from the Registry to a client.
pub enum RegResponse {
    Ok,
    Endpoint(u64),
    NotFound,
    List(Vec<String>),
}

fn handle(req: RegRequest) -> RegResponse {
    let mut guard = NAMES.lock();
    let names = guard.get_or_insert_with(BTreeMap::new);
    match req {
        RegRequest::Register { name, endpoint } => {
            names.insert(name, endpoint);
            RegResponse::Ok
        }
        RegRequest::Lookup { name } => match names.get(&name) {
            Some(&ep) => RegResponse::Endpoint(ep),
            None => RegResponse::NotFound,
        },
        RegRequest::List => RegResponse::List(names.keys().cloned().collect()),
    }
}

/// The Registry server task entry point.
pub extern "C" fn registry_server_task() -> ! {
    let ep = ipc::create_named_endpoint(REGISTRY_ENDPOINT);
    serial_println!("[registry] server online (endpoint {})", ep);

    loop {
        let msg = ipc::receive(ep);
        if msg.tag != TAG_REG {
            serial_println!("[registry] dropping message with tag {:#x}", msg.tag);
            continue;
        }
        let req_ptr = msg.words[0] as *mut RegRequest;
        if req_ptr.is_null() {
            continue;
        }
        let req = unsafe { *Box::from_raw(req_ptr) };
        let resp = handle(req);
        let resp_ptr = Box::into_raw(Box::new(resp)) as u64;
        ipc::reply(&msg, Message::with_words(TAG_REG, [resp_ptr, 0, 0, 0, 0, 0]));
    }
}

/// Client helper: perform a synchronous Registry RPC.
fn request(req: RegRequest) -> Option<RegResponse> {
    let ep = ipc::lookup(REGISTRY_ENDPOINT)?;
    let req_ptr = Box::into_raw(Box::new(req)) as u64;
    let reply = ipc::call(ep, Message::with_words(TAG_REG, [req_ptr, 0, 0, 0, 0, 0]));
    let resp_ptr = reply.words[0] as *mut RegResponse;
    if resp_ptr.is_null() {
        return None;
    }
    Some(unsafe { *Box::from_raw(resp_ptr) })
}

/// Register a service name -> endpoint mapping.
pub fn register(name: &str, endpoint: u64) -> bool {
    matches!(request(RegRequest::Register {
        name: String::from(name),
        endpoint,
    }), Some(RegResponse::Ok))
}

/// Resolve a service name to its endpoint id.
pub fn lookup(name: &str) -> Option<u64> {
    match request(RegRequest::Lookup {
        name: String::from(name),
    }) {
        Some(RegResponse::Endpoint(ep)) => Some(ep),
        _ => None,
    }
}

/// List all registered service names.
pub fn list() -> Vec<String> {
    match request(RegRequest::List) {
        Some(RegResponse::List(v)) => v,
        _ => Vec::new(),
    }
}
