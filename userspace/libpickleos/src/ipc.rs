//! IPC (Inter-Process Communication) abstractions for PICKLE OS.
//!
//! Provides high-level wrappers around the kernel's IPC primitives.

use crate::syscall;

/// An endpoint ID for IPC communication.
pub type EndpointId = u64;

/// Send a message to an endpoint.
pub fn send(endpoint: EndpointId, tag: u64) {
    syscall::sys_ipc_send(endpoint, tag);
}

/// Receive a message from an endpoint (blocking).
pub fn receive(endpoint: EndpointId) -> u64 {
    syscall::sys_ipc_recv(endpoint)
}

/// Call: send a message and wait for a reply.
/// This is a common pattern where a client sends a request and blocks for a response.
pub fn call(endpoint: EndpointId, request_tag: u64) -> u64 {
    send(endpoint, request_tag);
    receive(endpoint)
}

/// Reply to a message (just an alias for send for symmetry).
pub fn reply(endpoint: EndpointId, response_tag: u64) {
    send(endpoint, response_tag);
}
