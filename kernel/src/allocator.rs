//! Kernel heap allocator.
//!
//! Once paging is up we carve out a fixed virtual address range for the kernel
//! heap, map physical frames behind it, and register a `#[global_allocator]`.
//! After [`init_heap`] returns, the `alloc` crate's `Box`, `Vec`, `String`,
//! `BTreeMap`, etc. all work — which the scheduler, IPC and capability
//! subsystems rely on.
//!
//! The allocator itself is `linked_list_allocator::LockedHeap`: a real,
//! well-tested first-fit free-list allocator (not a bump/stub). It supports
//! both allocation and deallocation, so long-running tasks won't leak the heap.

use linked_list_allocator::LockedHeap;
use x86_64::{
    structures::paging::{
        mapper::MapToError, FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB,
    },
    VirtAddr,
};

/// Virtual address where the kernel heap begins. Chosen well clear of the
/// bootloader's mappings and the physical-memory map window.
pub const HEAP_START: usize = 0x_4444_4444_0000;
/// Heap size: 8 MiB. The kernel allocates each task's stack on the heap (see
/// `task::KSTACK_SIZE`), so the heap must comfortably hold every task stack plus
/// all other kernel data structures (window-manager back buffers, NextFS
/// caches, IPC messages, etc.). 8 MiB leaves generous head-room; QEMU gives us
/// 256 MiB of RAM, so mapping more frames here is cheap.
pub const HEAP_SIZE: usize = 8 * 1024 * 1024;

/// The global allocator instance. `LockedHeap` wraps the heap in a spinlock so
/// it is safe to allocate from any context.
#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// Map the heap pages and initialize the allocator over them.
pub fn init_heap(
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<(), MapToError<Size4KiB>> {
    // Compute the inclusive range of pages spanning [HEAP_START, HEAP_START+SIZE).
    let page_range = {
        let heap_start = VirtAddr::new(HEAP_START as u64);
        let heap_end = heap_start + HEAP_SIZE as u64 - 1u64;
        let heap_start_page = Page::containing_address(heap_start);
        let heap_end_page = Page::containing_address(heap_end);
        Page::range_inclusive(heap_start_page, heap_end_page)
    };

    // Map each heap page to a freshly allocated physical frame.
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
    for page in page_range {
        let frame = frame_allocator
            .allocate_frame()
            .ok_or(MapToError::FrameAllocationFailed)?;
        unsafe {
            mapper.map_to(page, frame, flags, frame_allocator)?.flush();
        }
    }

    // Hand the now-mapped region to the allocator.
    unsafe {
        ALLOCATOR.lock().init(HEAP_START as *mut u8, HEAP_SIZE);
    }

    Ok(())
}

/// Called when an allocation cannot be satisfied. On bare metal there's no way
/// to recover generically, so we panic with the failed layout for debugging.
#[alloc_error_handler]
fn alloc_error_handler(layout: alloc::alloc::Layout) -> ! {
    panic!("kernel heap allocation error: {:?}", layout);
}
