//! DMA pool allocator for device drivers.
//!
//! Many devices (AHCI, NVMe, network cards) need **physically contiguous**
//! memory for DMA buffers, command tables, and data structures that the device's
//! bus-master logic will read/write directly. Normal heap allocations (`Box`,
//! `Vec`) are virtually contiguous but not guaranteed physically contiguous, so
//! they cannot be used for DMA.
//!
//! This module provides a simple bump allocator over a reserved physical region.
//! A driver task requests DMA memory via a capability-checked interface, and the
//! kernel returns both the **virtual** address (for CPU access) and the
//! **physical** address (to program into device registers).
//!
//! ## Current design
//! - A fixed 8 MiB pool starting at physical address `DMA_POOL_PHYS_BASE`.
//! - Allocations are page-aligned and never freed (bump allocator).
//! - The pool is mapped into the kernel's virtual address space with normal
//!   cacheable flags (the CPU can read/write it), but drivers must ensure proper
//!   cache flushes / barriers when handing buffers to devices. A future
//!   enhancement would let drivers request non-cached mappings for certain buffers.
//!
//! ## Security
//! A capability model for DMA is TODO: currently any kernel task can allocate
//! DMA memory. A full implementation would mint `Object::DmaPool` capabilities
//! and check them here, or tie allocations to specific tasks so a compromised
//! driver can't exhaust the pool for others.

use crate::serial_println;
use spin::Mutex;
use x86_64::structures::paging::{FrameAllocator, Mapper, Page, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

/// Physical base of the DMA pool. We reserve a chunk of RAM that the bootloader
/// marks as usable. In a real system, this would be allocated dynamically from
/// the frame allocator, but for simplicity we hard-code a region.
///
/// IMPORTANT: this must sit well clear of the low physical memory the bootloader
/// uses for the kernel image, its page tables, and boot-info structures. The
/// original 16 MiB base collided with those structures — once the AHCI driver
/// zeroed its DMA buffers it silently corrupted live kernel memory (manifesting
/// as garbled serial output and a wedged kernel the moment real disks were
/// attached). 64 MiB is comfortably above the loaded kernel (QEMU gives us
/// 256 MiB), and the frame allocator additionally carves this range out so
/// nothing else can alias it.
pub const DMA_POOL_PHYS_BASE: u64 = 64 * 1024 * 1024; // 64 MiB
/// Size of the DMA pool. Public so the physical-frame allocator can carve this
/// range out of the usable memory it hands to the heap and task stacks — the
/// pool's physical frames must NOT be aliased by ordinary kernel allocations,
/// or DMA buffer zeroing would silently corrupt kernel data.
pub const DMA_POOL_SIZE: u64 = 8 * 1024 * 1024; // 8 MiB

/// Virtual base where the DMA pool is mapped in the kernel's address space.
/// We place it in the upper half, distinct from the MMIO region.
pub const DMA_POOL_VIRT_BASE: u64 = 0xFFFF_FF00_0000_0000;

/// Global DMA allocator state.
static DMA_ALLOCATOR: Mutex<Option<DmaAllocator>> = Mutex::new(None);

struct DmaAllocator {
    /// Next free offset (in bytes) within the pool.
    next_offset: u64,
    /// Total size of the pool.
    size: u64,
    /// Tracks whether a mark() session is currently active (for debug asserts).
    in_mark: bool,
}

impl DmaAllocator {
    fn new(size: u64) -> Self {
        Self {
            next_offset: 0,
            size,
            in_mark: false,
        }
    }

    /// Allocate `size` bytes (page-aligned) from the pool. Returns `(virt, phys)`
    /// or `None` if the pool is exhausted.
    fn allocate(&mut self, size: usize, align: usize) -> Option<(VirtAddr, PhysAddr)> {
        let size = size as u64;
        let align = align as u64;

        // Align the next offset up to the requested alignment.
        let offset = (self.next_offset + align - 1) & !(align - 1);
        if offset + size > self.size {
            return None; // pool exhausted
        }

        let phys = PhysAddr::new(DMA_POOL_PHYS_BASE + offset);
        let virt = VirtAddr::new(DMA_POOL_VIRT_BASE + offset);
        self.next_offset = offset + size;

        Some((virt, phys))
    }
}

/// Initialize the DMA pool: map the entire physical region into the kernel's
/// virtual address space with normal cacheable flags. Call once after the heap
/// and memory subsystem are up.
pub fn init() {
    crate::memory::with_memory(|mem| {
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        let pages = (DMA_POOL_SIZE / 4096) as usize;

        for i in 0..pages {
            let phys = PhysAddr::new(DMA_POOL_PHYS_BASE + i as u64 * 4096);
            let virt = VirtAddr::new(DMA_POOL_VIRT_BASE + i as u64 * 4096);
            let page = Page::<Size4KiB>::containing_address(virt);
            let frame = PhysFrame::containing_address(phys);

            unsafe {
                if let Err(e) = mem.mapper.map_to(page, frame, flags, &mut mem.frame_allocator) {
                    panic!("[dma] failed to map DMA pool page {}: {:?}", i, e);
                }
            }
        }
    });

    *DMA_ALLOCATOR.lock() = Some(DmaAllocator::new(DMA_POOL_SIZE));
    serial_println!(
        "[dma] initialized {} MiB pool at phys {:#x}, virt {:#x}",
        DMA_POOL_SIZE / (1024 * 1024),
        DMA_POOL_PHYS_BASE,
        DMA_POOL_VIRT_BASE
    );
}

/// Allocate physically contiguous DMA memory. Returns `(virt, phys, size_allocated)`.
/// The allocation is page-aligned and rounded up to the nearest page. The memory
/// is zeroed for safety (so stale data doesn't leak to devices).
///
/// A future enhancement: tie this to a capability so only authorized driver
/// tasks can allocate DMA memory, and track allocations per-task for accounting.
pub fn alloc_dma(size: usize) -> Option<(VirtAddr, PhysAddr, usize)> {
    let page_size = 4096;
    let align = page_size;
    let rounded = ((size + page_size - 1) / page_size) * page_size;

    let (virt, phys) = DMA_ALLOCATOR
        .lock()
        .as_mut()
        .expect("DMA allocator not initialized")
        .allocate(rounded, align)?;

    // Zero the memory for security (no stale kernel/user data visible to devices).
    unsafe {
        core::ptr::write_bytes(virt.as_mut_ptr::<u8>(), 0, rounded);
    }

    // (No per-allocation logging: transient DMA buffers are allocated on every
    // disk transfer, so logging here would flood the serial console. Use the
    // `dma` shell command to inspect pool usage.)
    Some((virt, phys, rounded))
}

/// Translate a physical address that lies within the DMA pool back to its
/// kernel virtual address. This is the inverse of the offset mapping set up in
/// [`init`]. Panics in debug builds if `phys` is outside the pool.
///
/// Drivers use this to obtain a CPU-accessible pointer to a buffer whose
/// physical address they previously handed to a device.
pub fn phys_to_virt(phys: u64) -> VirtAddr {
    debug_assert!(
        phys >= DMA_POOL_PHYS_BASE && phys < DMA_POOL_PHYS_BASE + DMA_POOL_SIZE,
        "phys_to_virt: address outside DMA pool"
    );
    VirtAddr::new(DMA_POOL_VIRT_BASE + (phys - DMA_POOL_PHYS_BASE))
}

/// Record the current allocation high-water mark. Pair with [`reset_to`] to
/// reclaim every transient allocation made after this point — the DMA pool is a
/// bump allocator, so this gives cheap arena-style scoping for short-lived
/// buffers (e.g. AHCI command tables and read/write bounce buffers) without a
/// full free list. Persistent allocations (made *before* the mark) are
/// untouched. Without this, every disk transfer would leak a buffer and the
/// pool would exhaust within a few seconds of real disk I/O.
pub fn mark() -> u64 {
    let mut alloc = DMA_ALLOCATOR.lock();
    if let Some(a) = alloc.as_mut() {
        // debug_assert: detect nested mark() calls (mark without intervening reset_to).
        debug_assert!(!a.in_mark, "mark() called while another mark() session is active");
        a.in_mark = true;
        a.next_offset
    } else {
        0
    }
}

/// Roll the bump pointer back to a previously recorded [`mark`], freeing all
/// allocations made since. Only moves the pointer backwards (a stale/forward
/// mark is ignored), so it can never hand out memory that is still live.
pub fn reset_to(mark: u64) {
    if let Some(a) = DMA_ALLOCATOR.lock().as_mut() {
        debug_assert!(a.in_mark, "reset_to() called without a prior mark()");
        a.in_mark = false;
        if mark <= a.next_offset {
            a.next_offset = mark;
        }
    }
}

/// Return DMA pool usage statistics: (bytes used, bytes total).
pub fn stats() -> (u64, u64) {
    DMA_ALLOCATOR
        .lock()
        .as_ref()
        .map(|a| (a.next_offset, a.size))
        .unwrap_or((0, 0))
}
