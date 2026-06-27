//! Capability abstractions for PICKLE OS.
//!
//! Provides types and functions for working with capabilities.

use crate::syscall;

/// A capability slot identifier.
pub type CapSlot = usize;

/// Rights that can be held on a capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rights(pub u32);

impl Rights {
    pub const NONE: Rights = Rights(0);
    pub const READ: Rights = Rights(1 << 0);
    pub const WRITE: Rights = Rights(1 << 1);
    pub const EXECUTE: Rights = Rights(1 << 2);
    pub const ALL: Rights = Rights(0xFFFF_FFFF);
}

/// Check if the current process has a capability with the specified rights.
pub fn has_capability(slot: CapSlot, rights: Rights) -> bool {
    syscall::sys_cap_check(slot, rights.0)
}
