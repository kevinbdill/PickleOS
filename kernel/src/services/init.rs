//! Init server: the bootstrapper for PICKLE OS core services.
//!
//! Init is the first service task. It brings up the Registry and VFS servers in
//! dependency order, waits for each to publish its IPC endpoint, then records
//! them in the Registry so other components can discover them by name. In a
//! fully user-space configuration Init would also hold the root capability table
//! and hand derived capabilities to each service; here it performs the
//! equivalent bootstrap and announces readiness.

use super::{registry, vfs};
use crate::ipc;
use crate::serial_println;
use crate::task;

/// Spin-wait (yielding) until a named endpoint becomes available, so we don't
/// race ahead of a server that hasn't registered yet.
fn wait_for_endpoint(name: &str) -> u64 {
    loop {
        if let Some(ep) = ipc::lookup(name) {
            return ep;
        }
        task::yield_now();
    }
}

/// The Init server task entry point.
pub extern "C" fn init_server_task() -> ! {
    serial_println!("[init] bootstrapping core services...");

    // Bring up the Registry first; everything else registers with it.
    task::spawn_kernel_task("registry", registry::registry_server_task);
    let reg_ep = wait_for_endpoint(registry::REGISTRY_ENDPOINT);
    serial_println!("[init] registry up (endpoint {})", reg_ep);

    // Bring up the VFS server (it seeds MemFS on start).
    task::spawn_kernel_task("vfs", vfs::vfs_server_task);
    let vfs_ep = wait_for_endpoint(vfs::VFS_ENDPOINT);
    serial_println!("[init] vfs up (endpoint {})", vfs_ep);

    // Publish services in the Registry by name.
    registry::register(registry::REGISTRY_ENDPOINT, reg_ep);
    registry::register(vfs::VFS_ENDPOINT, vfs_ep);

    let services = registry::list();
    serial_println!(
        "[init] all core services online; registry knows {} service(s):",
        services.len()
    );
    for name in &services {
        serial_println!("[init]   - {}", name);
    }
    serial_println!("[init] userland ready; type 'help' in the shell.");

    // Run an end-to-end self-test of the VFS path so the service stack is
    // verifiable on a headless boot (the interactive shell needs a keyboard).
    task::spawn_kernel_task("vfs-selftest", super::vfs_selftest_task);

    // Init lives on as the parent/reaper. Nothing to do yet, so idle politely.
    loop {
        task::yield_now();
    }
}
