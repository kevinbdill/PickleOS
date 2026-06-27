//! Capability system.
//!
//! PICKLE OS is **capability-based**: instead of an ambient-authority model where
//! any task can name any object and access is checked against a global ACL, a
//! task can only act on a kernel object if it holds an unforgeable
//! **capability** to it. A capability bundles:
//!   * a reference to a kernel object (an IPC endpoint, a memory region, ...),
//!   * a set of **rights** (send, receive, read, write, grant).
//!
//! Capabilities live in a per-task **capability table** and are named by a
//! small integer index (a "cap slot") — much like a file descriptor, but the
//! integer is meaningless without the table, so it cannot be forged across
//! tasks. Authority is transferred explicitly by *granting* (copying, possibly
//! with reduced rights) a capability from one table into another.
//!
//! This module enforces the core invariant of capability systems:
//! **rights are monotonically non-increasing on derivation** — you can never
//! mint a capability with more authority than the one you derived it from.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use spin::Mutex;

/// The kinds of kernel objects a capability can point at. Extend this as the
/// kernel grows (threads, address spaces, IRQ handlers, device MMIO, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Object {
    /// An IPC endpoint (see [`crate::ipc`]).
    Endpoint(crate::ipc::EndpointId),
    /// A region of physical/virtual memory `[base, base+len)`.
    Memory { base: u64, len: u64 },
    /// A hardware IRQ line (for user-space driver tasks).
    Irq(u8),
    /// A range of x86 I/O ports `[base, base+count)` (for driver tasks that
    /// talk to legacy port-mapped devices, e.g. the PS/2 controller at 0x60).
    Port { base: u16, count: u16 },
    /// A memory-mapped I/O region `[phys_base, phys_base+len)`. Modern devices
    /// (AHCI, NVMe, GPUs, NICs) expose control registers as physical memory.
    /// A driver holding this cap can read/write the region; the kernel maps it
    /// with cache-disable + write-through flags so MMIO semantics are preserved.
    Mmio { phys_base: u64, len: u64 },
    /// A named role/right for ambient authority that doesn't map to a concrete
    /// kernel object — e.g. SPAWN, KILL, FILE_SYSTEM, NETWORK. The u64 is a
    /// role identifier assigned by convention:
    ///   1 = SPAWN
    ///   2 = FILE_SYSTEM
    ///   3 = SYS_KILL
    ///   4 = NETWORK
    ///   5 = WINDOW_SERVER
    /// Tasks minted at boot get these roles; user tasks must be granted them.
    Role(u64),
}

/// Access rights carried by a capability. Implemented as a simple bitset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rights(pub u32);

impl Rights {
    pub const NONE: Rights = Rights(0);
    pub const SEND: Rights = Rights(1 << 0); // may send on an endpoint
    pub const RECV: Rights = Rights(1 << 1); // may receive on an endpoint
    pub const READ: Rights = Rights(1 << 2); // may read a memory region
    pub const WRITE: Rights = Rights(1 << 3); // may write a memory region
    pub const GRANT: Rights = Rights(1 << 4); // may grant/derive this cap to others
    /// All rights (used when the kernel mints a root capability).
    pub const ALL: Rights = Rights(0b11111);

    /// Union of two right sets.
    pub const fn union(self, other: Rights) -> Rights {
        Rights(self.0 | other.0)
    }
    /// Intersection — used to clamp derived rights to a subset.
    pub const fn intersect(self, other: Rights) -> Rights {
        Rights(self.0 & other.0)
    }
    /// True if `self` contains *all* the bits in `needed`.
    pub const fn contains(self, needed: Rights) -> bool {
        (self.0 & needed.0) == needed.0
    }
}

// Role identifiers used with Object::Role for ambient-rights checking.
pub const ROLE_SPAWN: u64 = 1;
pub const ROLE_FILE_SYSTEM: u64 = 2;
pub const ROLE_KILL: u64 = 3;
pub const ROLE_NETWORK: u64 = 4;
pub const ROLE_WINDOW_SERVER: u64 = 5;

/// A single capability: an object + the rights the holder has over it.
#[derive(Debug, Clone, Copy)]
pub struct Capability {
    pub object: Object,
    pub rights: Rights,
}

/// A per-task table of capabilities, indexed by cap slot.
struct CapTable {
    /// `slots[i]` is `Some(cap)` if slot `i` holds a capability.
    slots: Vec<Option<Capability>>,
}

impl CapTable {
    fn new() -> Self {
        CapTable { slots: Vec::new() }
    }

    /// Insert a capability into the first free slot and return that slot index.
    fn insert(&mut self, cap: Capability) -> usize {
        if let Some(i) = self.slots.iter().position(|s| s.is_none()) {
            self.slots[i] = Some(cap);
            i
        } else {
            self.slots.push(Some(cap));
            self.slots.len() - 1
        }
    }
}

/// Global map of task id -> capability table.
static TABLES: Mutex<Option<BTreeMap<u64, CapTable>>> = Mutex::new(None);

/// Initialize the capability subsystem (needs the heap).
pub fn init() {
    *TABLES.lock() = Some(BTreeMap::new());
}

fn with_tables<R>(f: impl FnOnce(&mut BTreeMap<u64, CapTable>) -> R) -> R {
    // Interrupts off while the lock is held — see the note on `ipc::with_state`.
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut guard = TABLES.lock();
        let t = guard.as_mut().expect("capability system not initialized");
        f(t)
    })
}

/// Ensure a task has a (possibly empty) capability table.
pub fn create_table(task_id: u64) {
    with_tables(|t| {
        t.entry(task_id).or_insert_with(CapTable::new);
    });
}

/// Mint a fresh root capability directly into `task_id`'s table. Only the kernel
/// calls this (e.g. to give the init task authority over a device). Returns the
/// new cap slot.
pub fn mint(task_id: u64, object: Object, rights: Rights) -> usize {
    with_tables(|t| {
        let table = t.entry(task_id).or_insert_with(CapTable::new);
        table.insert(Capability { object, rights })
    })
}

/// Find the first capability in `task_id`'s table whose object satisfies
/// `pred` and which carries at least `needed` rights. Used by device-driver
/// authorization (port-IO / IRQ) where the object is identified by its content
/// (a port range or IRQ number) rather than a fixed slot index.
pub fn find_object<F: Fn(&Object) -> bool>(
    task_id: u64,
    needed: Rights,
    pred: F,
) -> Option<Capability> {
    with_tables(|t| {
        t.get(&task_id).and_then(|table| {
            table
                .slots
                .iter()
                .flatten()
                .find(|cap| cap.rights.contains(needed) && pred(&cap.object))
                .copied()
        })
    })
}

/// Look up the capability in `slot` of `task_id`'s table.
pub fn lookup(task_id: u64, slot: usize) -> Option<Capability> {
    with_tables(|t| {
        t.get(&task_id)
            .and_then(|table| table.slots.get(slot).copied().flatten())
    })
}

/// Check that `task_id` holds a capability in `slot` with at least `needed`
/// rights. This is the function the syscall layer calls before honoring a
/// request that names a kernel object by cap slot.
pub fn check(task_id: u64, slot: usize, needed: Rights) -> bool {
    match lookup(task_id, slot) {
        Some(cap) => cap.rights.contains(needed),
        None => false,
    }
}

/// Check that `task_id` holds a capability for a given `role` (see
/// [`Object::Role`]) with at least `needed` rights. Convenience wrapper around
/// [`find_object`].
pub fn check_role(task_id: u64, role: u64, needed: Rights) -> bool {
    find_object(task_id, needed, |obj| matches!(obj, Object::Role(r) if *r == role)).is_some()
}

/// **Capability derivation / grant.** Copy the capability in `from_slot` of
/// `from_task`'s table into `to_task`'s table, clamping its rights to
/// `new_rights` (which must be a subset of the source rights). The source must
/// itself hold the [`Rights::GRANT`] right.
///
/// Returns the new cap slot in the destination table, or `None` if the grant
/// is not permitted. This enforces the monotonic-rights invariant: you cannot
/// amplify authority by granting.
pub fn grant(
    from_task: u64,
    from_slot: usize,
    to_task: u64,
    new_rights: Rights,
) -> Option<usize> {
    let source = lookup(from_task, from_slot)?;
    // Must be allowed to grant, and may only pass on rights we already have.
    if !source.rights.contains(Rights::GRANT) {
        return None;
    }
    let clamped = source.rights.intersect(new_rights);
    with_tables(|t| {
        let table = t.entry(to_task).or_insert_with(CapTable::new);
        Some(table.insert(Capability {
            object: source.object,
            rights: clamped,
        }))
    })
}

/// Remove the capability in `slot` of `task_id`'s table (revocation of the local
/// reference). Returns the removed capability if present.
pub fn revoke(task_id: u64, slot: usize) -> Option<Capability> {
    with_tables(|t| {
        t.get_mut(&task_id).and_then(|table| {
            table.slots.get_mut(slot).and_then(|s| s.take())
        })
    })
}
