//! Memory management for user-space programs.

use linked_list_allocator::LockedHeap;
use crate::syscall;

/// The global heap allocator for user-space programs.
#[global_allocator]
pub static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// Starting address for the user heap.
/// This is in user space (below 0x0000_8000_0000_0000).
pub const HEAP_START: usize = 0x0000_5000_0000_0000;

/// Map anonymous memory at the specified address.
pub fn mmap(addr: u64, len: usize, prot: u32) -> Result<usize, &'static str> {
    syscall::sys_mmap(addr, len, prot)
}

/// Unmap memory at the specified address.
pub fn munmap(addr: u64, len: usize) -> Result<(), &'static str> {
    syscall::sys_munmap(addr, len)
}
