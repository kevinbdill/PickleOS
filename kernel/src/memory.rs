//! Virtual memory management: paging and physical frame allocation.
//!
//! The bootloader has already enabled 4-level paging and (thanks to the
//! `map_physical_memory` feature) mapped *all* of physical RAM into the virtual
//! address space at a known `physical_memory_offset`. That lets us read and
//! write any physical frame from kernel virtual addresses.
//!
//! This module provides:
//!   * [`init`] — builds an [`OffsetPageTable`] over the active page tables so
//!     we can create new mappings.
//!   * [`BootInfoFrameAllocator`] — hands out unused physical frames, sourced
//!     from the bootloader's memory map.
//!   * A global mapper + allocator ([`install_global`], [`with_memory`]) so any
//!     subsystem (heap, task stacks, user processes) can map memory later.
//!   * User-space page table creation and management for process isolation.

use crate::serial_println;
use alloc::vec::Vec;
use bootloader_api::info::{MemoryRegion, MemoryRegionKind, MemoryRegions};
use spin::Mutex;
use x86_64::{
    structures::paging::{
        FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame,
        Size4KiB,
    },
    PhysAddr, VirtAddr,
};

/// Start of the user heap region (will be expanded via sbrk in the future).
pub const USER_HEAP_START: u64 = 0x0000_0001_0000_0000;

/// The flag bit used in L1 page table entries to mark Copy-on-Write pages.
/// Bit 9 is available to the OS per the x86_64 specification — it is ignored
/// by the CPU for page-table walks and never set by the hardware.
pub const COW_FLAG: PageTableFlags = PageTableFlags::BIT_9;

/// Build a mapper for the currently-active level-4 page table.
///
/// # Safety
/// The caller must guarantee that all of physical memory is mapped at
/// `physical_memory_offset` and that this is called only once.
pub unsafe fn init(physical_memory_offset: VirtAddr) -> OffsetPageTable<'static> {
    let level_4_table = active_level_4_table(physical_memory_offset);
    OffsetPageTable::new(level_4_table, physical_memory_offset)
}

/// Return a mutable reference to the active level-4 page table by reading the
/// CR3 register and translating the physical frame to a virtual address.
///
/// # Safety
/// Same requirements as [`init`]; aliasing the page table is UB if misused.
unsafe fn active_level_4_table(physical_memory_offset: VirtAddr) -> &'static mut PageTable {
    use x86_64::registers::control::Cr3;

    let (level_4_table_frame, _) = Cr3::read();
    let phys = level_4_table_frame.start_address();
    let virt = physical_memory_offset + phys.as_u64();
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();
    &mut *page_table_ptr
}

/// A [`FrameAllocator`] that returns usable frames from the bootloader memory
/// map. Simple but real: it lazily iterates usable regions and never reuses a
/// frame (no free list yet — see TODO).
pub struct BootInfoFrameAllocator {
    /// The bootloader's memory regions as a plain slice. Stored as a slice
    /// (rather than `&MemoryRegions`, which wraps a raw pointer and is not
    /// `Sync`) so the allocator can live inside a global `Mutex`.
    memory_map: &'static [MemoryRegion],
    next: usize,
}

impl BootInfoFrameAllocator {
    /// Create an allocator from the memory regions list (bootloader 0.11).
    ///
    /// # Safety
    /// The caller must guarantee the passed memory regions are valid and that
    /// frames marked `Usable` are actually unused.
    pub unsafe fn init(memory_map: &'static MemoryRegions) -> Self {
        BootInfoFrameAllocator {
            memory_map: &memory_map[..],
            next: 0,
        }
    }

    /// Iterator over all usable 4 KiB frames described by the memory regions.
    ///
    /// The DMA pool (see [`crate::driver::dma`]) hard-codes a fixed physical
    /// range so device drivers get physically-contiguous, identity-known
    /// buffers. Those frames are *also* marked `Usable` in the bootloader memory
    /// map, so we must explicitly carve them out here — otherwise the heap and
    /// task stacks would be handed the very same physical frames the DMA pool
    /// uses, and zeroing a DMA buffer would silently corrupt live kernel memory.
    fn usable_frames(&self) -> impl Iterator<Item = PhysFrame> + '_ {
        const DMA_LO: u64 = crate::driver::dma::DMA_POOL_PHYS_BASE;
        const DMA_HI: u64 = crate::driver::dma::DMA_POOL_PHYS_BASE + crate::driver::dma::DMA_POOL_SIZE;

        let regions = self.memory_map.iter();
        let usable_regions = regions.filter(|r| r.kind == MemoryRegionKind::Usable);
        let addr_ranges = usable_regions.map(|r| r.start..r.end);
        let frame_addresses = addr_ranges.flat_map(|r| r.step_by(4096));
        frame_addresses
            // Reserve the DMA pool's physical frames for the DMA allocator only.
            .filter(|&addr| addr < DMA_LO || addr >= DMA_HI)
            .map(|addr| PhysFrame::containing_address(PhysAddr::new(addr)))
    }
}

unsafe impl FrameAllocator<Size4KiB> for BootInfoFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        // TODO: track freed frames in a free list / bitmap so `deallocate_frame`
        // can recycle memory. For a bring-up allocator, monotonic is fine.
        let frame = self.usable_frames().nth(self.next);
        self.next += 1;
        frame
    }
}

/// Global handle to the active mapper + frame allocator, set up once during
/// init. Subsystems use [`with_memory`] to map/unmap pages after boot.
pub struct MemoryManager {
    pub mapper: OffsetPageTable<'static>,
    pub frame_allocator: BootInfoFrameAllocator,
    pub physical_memory_offset: VirtAddr,
    /// Storage for user-space page tables (indexed by task ID or similar).
    pub user_page_tables: Vec<PhysAddr>,
}

static MEMORY: Mutex<Option<MemoryManager>> = Mutex::new(None);

/// Store the mapper + allocator globally for later use by other subsystems.
pub fn install_global(
    mapper: OffsetPageTable<'static>,
    frame_allocator: BootInfoFrameAllocator,
    physical_memory_offset: VirtAddr,
) {
    *MEMORY.lock() = Some(MemoryManager {
        mapper,
        frame_allocator,
        physical_memory_offset,
        user_page_tables: Vec::new(),
    });
}

/// Run a closure with mutable access to the global memory manager. Panics if
/// called before [`install_global`].
pub fn with_memory<R>(f: impl FnOnce(&mut MemoryManager) -> R) -> R {
    let mut guard = MEMORY.lock();
    let mm = guard.as_mut().expect("memory manager not initialized");
    f(mm)
}

/// Allocate `count` contiguous-in-virtual-space writable pages starting at
/// `start`, backed by (not necessarily contiguous) physical frames. Returns
/// the starting virtual address. Used to grow task/kernel stacks and the heap.
pub fn map_region(start: VirtAddr, count: u64, flags: PageTableFlags) -> VirtAddr {
    with_memory(|mm| {
        for i in 0..count {
            let page: Page<Size4KiB> = Page::containing_address(start + i * 4096u64);
            let frame = mm
                .frame_allocator
                .allocate_frame()
                .expect("out of physical frames");
            unsafe {
                mm.mapper
                    .map_to(page, frame, flags, &mut mm.frame_allocator)
                    .expect("map_to failed")
                    .flush();
            }
        }
    });
    start
}

/// Convenience flag set for ordinary writable kernel memory.
pub fn kernel_rw_flags() -> PageTableFlags {
    PageTableFlags::PRESENT | PageTableFlags::WRITABLE
}

/// Convenience flag set for user-accessible writable memory (ring 3).
pub fn user_rw_flags() -> PageTableFlags {
    PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE
}

/// Recursively deep-copy a page-table sub-tree, duplicating the table *pages*
/// at every level down to L1 but **sharing the leaf data frames**.
///
/// `level` is the level of `src` (4=PML4, 3=PDPT, 2=PD, 1=PT). For each present
/// entry:
///   * at L1, or any entry marked `HUGE_PAGE`, the entry is copied verbatim so
///     the new tree points at the *same* physical frame (shared mapping);
///   * otherwise the child table is deep-copied and the new entry re-pointed at
///     the freshly allocated child.
///
/// The result is a private copy of the paging *structure* — mappings can be
/// added/removed in the copy without disturbing the source — while the actual
/// backing memory (kernel code/data) stays shared, which is exactly what we
/// want for kernel low-half mappings that must remain valid in every process.
fn deep_copy_table(
    level: u8,
    src: &PageTable,
    fa: &mut BootInfoFrameAllocator,
    offset: VirtAddr,
) -> PhysFrame {
    let new_frame = fa.allocate_frame().expect("out of memory: deep_copy_table");
    let new_ptr = (offset + new_frame.start_address().as_u64()).as_mut_ptr::<PageTable>();
    unsafe { core::ptr::write_bytes(new_ptr, 0, 1) };
    let dst = unsafe { &mut *new_ptr };

    for i in 0..512 {
        let e = &src[i];
        if e.is_unused() {
            continue;
        }
        if level == 1 || e.flags().contains(PageTableFlags::HUGE_PAGE) {
            // Leaf (or huge-page) mapping: share the backing frame verbatim.
            dst[i] = e.clone();
        } else {
            // Interior entry: recurse and re-point at the private child table.
            let child = unsafe { &*((offset + e.addr().as_u64()).as_ptr::<PageTable>()) };
            let child_frame = deep_copy_table(level - 1, child, fa, offset);
            dst[i].set_addr(child_frame.start_address(), e.flags());
        }
    }

    new_frame
}

impl MemoryManager {
    /// Create a fresh user-space page table by cloning the kernel mappings.
    ///
    /// This gives each user process its own isolated address space while keeping
    /// the kernel mapped (so syscalls/interrupts work). The upper half (kernel)
    /// is shared; the lower half (user) is empty and will be filled by the ELF
    /// loader.
    pub fn create_user_mapper(&mut self) -> OffsetPageTable<'static> {
        let offset = self.physical_memory_offset;

        // Allocate and zero a frame for the new level-4 page table.
        let new_l4_frame = self
            .frame_allocator
            .allocate_frame()
            .expect("out of memory for user page table");
        let l4_ptr = (offset + new_l4_frame.start_address().as_u64()).as_mut_ptr::<PageTable>();
        unsafe { core::ptr::write_bytes(l4_ptr, 0, 1) };
        let new_l4 = unsafe { &mut *l4_ptr };

        // Always clone from the KERNEL's original CR3 (saved at scheduler init),
        // never the live CR3 — the live CR3 may belong to a user process that
        // already has user-half mappings, which would leak into the new space.
        let kernel_l4 = unsafe {
            let kernel_cr3 = crate::task::scheduler::kernel_cr3()
                .expect("kernel CR3 not saved at scheduler init");
            &*((offset + kernel_cr3.as_u64()).as_ptr::<PageTable>())
        };

        // ADDRESS-SPACE ISOLATION
        // ------------------------
        // The kernel maps its high half (physical-memory window, heap, etc.)
        // through upper L4 entries, and its low code/data plus ALL user regions
        // (code @ 4 MiB, mmap heap @ 4 GiB, stack near 512 GiB) through L4[0].
        //
        // For entries other than L4[0] we copy the kernel entry verbatim: those
        // sub-trees are kernel-global and must stay shared (so syscalls and
        // interrupts see the same kernel everywhere).
        //
        // For L4[0] we DEEP-COPY the entire sub-tree's page-table *pages* (L3,
        // L2, L1), sharing only the leaf data frames. Each process therefore
        // gets private mapping structure for the user half: when the ELF loader
        // maps user pages, it mutates this process's private tables only — other
        // processes are unaffected. Kernel low-half mappings are preserved
        // because we copy the kernel's existing entries into the private tables.
        for i in 0..512 {
            new_l4[i] = kernel_l4[i].clone();
        }

        let k0 = &kernel_l4[0];
        if !k0.is_unused() && !k0.flags().contains(PageTableFlags::HUGE_PAGE) {
            let src_l3 = unsafe { &*((offset + k0.addr().as_u64()).as_ptr::<PageTable>()) };
            let new_l3_frame = deep_copy_table(3, src_l3, &mut self.frame_allocator, offset);
            new_l4[0].set_addr(new_l3_frame.start_address(), k0.flags());
        }

        unsafe { OffsetPageTable::new(new_l4, offset) }
    }

    /// Create a Copy-on-Write fork of a user address space.
    ///
    /// Instead of deep-copying every user page (the O(n) bottleneck), this
    /// shares all user frames between parent and child by:
    ///   1. Clearing the WRITABLE bit on every parent user page that *was*
    ///      writable, and setting the COW flag (BIT_9) to mark it.
    ///   2. Creating a fresh address-space *structure* for the child (via
    ///      [`create_user_mapper`]), then copying each parent L1 entry into the
    ///      child's tables verbatim — both now point at the same physical frame
    ///      with the same (read-only + COW) flags.
    ///
    /// On the next write to any shared page, the CPU raises a page fault.
    /// The page fault handler detects the COW flag, allocates a fresh frame,
    /// copies the data, and maps it writable without COW in the faulting task's
    /// address space. Pages that were genuinely read-only in the parent stay
    /// shared read-only forever (no COW flag).
    ///
    /// The parent's TLB is flushed for each PTE that was made read-only so the
    /// parent sees the new permissions immediately. Returns the child's L4
    /// physical address (its CR3 value), recorded in `user_page_tables`.
    pub fn cow_fork_address_space(&mut self, parent_cr3: PhysAddr) -> PhysAddr {
        let offset = self.physical_memory_offset;

        // ---- Step 1: Walk the parent's user pages. For each writable leaf,
        //      clear WRITABLE and set COW_FLAG in place. ----
        //
        // We also record every leaf entry so we can replicate it in the child.
        // Form: (virtual_address, raw_pte_u64).
        let mut user_leaves: Vec<(u64, u64)> = Vec::new();

        let l4 = unsafe { &mut *((offset + parent_cr3.as_u64()).as_mut_ptr::<PageTable>()) };
        for i4 in 0..256usize {
            let e4 = &l4[i4];
            if e4.is_unused() || e4.flags().contains(PageTableFlags::HUGE_PAGE) {
                continue;
            }
            let l3 = unsafe { &mut *((offset + e4.addr().as_u64()).as_mut_ptr::<PageTable>()) };
            for i3 in 0..512 {
                let e3 = &l3[i3];
                if e3.is_unused() || e3.flags().contains(PageTableFlags::HUGE_PAGE) {
                    continue;
                }
                let l2 = unsafe { &mut *((offset + e3.addr().as_u64()).as_mut_ptr::<PageTable>()) };
                for i2 in 0..512 {
                    let e2 = &l2[i2];
                    if e2.is_unused() || e2.flags().contains(PageTableFlags::HUGE_PAGE) {
                        continue;
                    }
                    let l1 =
                        unsafe { &mut *((offset + e2.addr().as_u64()).as_mut_ptr::<PageTable>()) };
                    for i1 in 0..512 {
                        let e1 = &l1[i1];
                        if e1.is_unused()
                            || !e1.flags().contains(PageTableFlags::USER_ACCESSIBLE)
                        {
                            continue;
                        }

                        let vaddr = ((i4 as u64) << 39)
                            | ((i3 as u64) << 30)
                            | ((i2 as u64) << 21)
                            | ((i1 as u64) << 12);

                        let flags = e1.flags();
                        let raw_entry = e1.addr().as_u64() | flags.bits();

                        if flags.contains(PageTableFlags::WRITABLE) {
                            // Make the parent's PTE read-only + COW.
                            let cow_addr = e1.addr();
                            let cow_flags = (flags & !PageTableFlags::WRITABLE) | COW_FLAG;
                            l1[i1].set_addr(cow_addr, cow_flags);
                            // Flush this page from the TLB so the parent sees
                            // the new read-only mapping immediately.
                            unsafe {
                                x86_64::instructions::tlb::flush(VirtAddr::new(vaddr));
                            }
                            user_leaves.push((vaddr, cow_addr.as_u64() | cow_flags.bits()));
                        } else {
                            // Already read-only: share as-is, no COW flag.
                            user_leaves.push((vaddr, raw_entry));
                        }
                    }
                }
            }
        }

        // ---- Step 2: Create a fresh address space for the child. ----
        let mut child_mapper = self.create_user_mapper();

        // ---- Step 3: Map each shared leaf in the child. ----
        // For each leaf we (a) call map_to to create intermediate page-table
        // pages (L3/L2) if they don't exist, then (b) overwrite the L1 entry
        // with the verbatim PTE.
        for (vaddr, raw_entry) in user_leaves {
            let page = Page::<Size4KiB>::containing_address(VirtAddr::new(vaddr));
            let frame_addr = PhysAddr::new(raw_entry & 0x000f_ffff_ffff_f000);
            let frame = PhysFrame::containing_address(frame_addr);

            // (a) Create intermediate tables.
            unsafe {
                let temp_flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
                match child_mapper.map_to(page, frame, temp_flags, &mut self.frame_allocator) {
                    Ok(flush) => flush.ignore(),
                    Err(e) => {
                        serial_println!(
                            "memory :: cow_fork: map_to failed for {:#x}: {:?}",
                            vaddr,
                            e
                        );
                        continue;
                    }
                }
            }

            // (b) Overwrite the leaf PTE with the shared entry.
            let child_l4: &mut PageTable = child_mapper.level_4_table();
            let child_l4_virt = VirtAddr::from_ptr(child_l4);
            let child_l4_phys_addr = child_l4_virt.as_u64() - offset.as_u64();
            let child_l4_table =
                unsafe { &mut *((offset + child_l4_phys_addr).as_mut_ptr::<PageTable>()) };
            let ci4 = (vaddr >> 39) & 0x1FF;
            let ci3 = (vaddr >> 30) & 0x1FF;
            let ci2 = (vaddr >> 21) & 0x1FF;
            let ci1 = (vaddr >> 12) & 0x1FF;

            let ce3_addr = child_l4_table[ci4 as usize].addr();
            let ce3_table =
                unsafe { &mut *((offset + ce3_addr.as_u64()).as_mut_ptr::<PageTable>()) };
            let ce2_addr = ce3_table[ci3 as usize].addr();
            let ce2_table =
                unsafe { &mut *((offset + ce2_addr.as_u64()).as_mut_ptr::<PageTable>()) };
            let ce1_addr = ce2_table[ci2 as usize].addr();
            let ce1_table =
                unsafe { &mut *((offset + ce1_addr.as_u64()).as_mut_ptr::<PageTable>()) };

            // Write the exact raw PTE bits — this preserves COW_FLAG and the lack of
            // WRITABLE, so the child also sees the page as read-only + COW.
            let raw_addr = PhysAddr::new(raw_entry & 0x000f_ffff_ffff_f000);
            let raw_flags = PageTableFlags::from_bits_truncate(raw_entry & !0x000f_ffff_ffff_f000);
            ce1_table[ci1 as usize].set_addr(raw_addr, raw_flags);
        }

        // ---- Step 4: Register and return the child's CR3. ----
        self.store_user_mapper(child_mapper)
    }

    /// Duplicate a user address space for `fork`.
    ///
    /// Creates a brand-new address space (kernel mappings shared, user half
    /// empty — exactly like [`create_user_mapper`]) and then copies **every**
    /// user-accessible page mapped in `parent_cr3` into it. Each page gets a
    /// freshly allocated physical frame whose contents are byte-copied from the
    /// parent, so the child is a fully independent snapshot (no copy-on-write
    /// yet). Returns the child's L4 physical address (its CR3), which is also
    /// recorded in `user_page_tables`.
    pub fn duplicate_user_address_space(&mut self, parent_cr3: PhysAddr) -> PhysAddr {
        let offset = self.physical_memory_offset;

        // 1. Walk the parent's tables and collect every user (USER_ACCESSIBLE)
        //    leaf mapping as (virtual address, source frame, flags). We gather
        //    first, then map, to avoid holding references into the parent tree
        //    while mutating the child via the frame allocator.
        let mut user_pages: Vec<(u64, PhysAddr, PageTableFlags)> = Vec::new();

        // User space is the lower canonical half (L4 indices 0..256); the kernel
        // owns the higher half. We only need to scan the lower half. At interior
        // levels we descend through every PRESENT (non-huge) entry and rely on
        // the *leaf* USER_ACCESSIBLE bit to distinguish genuine user pages from
        // the kernel's low-half code/data (which is mapped without the user bit).
        let l4 = unsafe { &*((offset + parent_cr3.as_u64()).as_ptr::<PageTable>()) };
        for i4 in 0..256usize {
            let e4 = &l4[i4];
            if e4.is_unused() || e4.flags().contains(PageTableFlags::HUGE_PAGE) {
                continue;
            }
            let l3 = unsafe { &*((offset + e4.addr().as_u64()).as_ptr::<PageTable>()) };
            for i3 in 0..512 {
                let e3 = &l3[i3];
                if e3.is_unused() || e3.flags().contains(PageTableFlags::HUGE_PAGE) {
                    continue; // 1 GiB pages unsupported here.
                }
                let l2 = unsafe { &*((offset + e3.addr().as_u64()).as_ptr::<PageTable>()) };
                for i2 in 0..512 {
                    let e2 = &l2[i2];
                    if e2.is_unused() || e2.flags().contains(PageTableFlags::HUGE_PAGE) {
                        continue; // 2 MiB pages unsupported here.
                    }
                    let l1 = unsafe { &*((offset + e2.addr().as_u64()).as_ptr::<PageTable>()) };
                    for i1 in 0..512 {
                        let e1 = &l1[i1];
                        // Only copy genuine ring-3 pages.
                        if e1.is_unused()
                            || !e1.flags().contains(PageTableFlags::USER_ACCESSIBLE)
                        {
                            continue;
                        }
                        let vaddr = ((i4 as u64) << 39)
                            | ((i3 as u64) << 30)
                            | ((i2 as u64) << 21)
                            | ((i1 as u64) << 12);
                        user_pages.push((vaddr, e1.addr(), e1.flags()));
                    }
                }
            }
        }

        // 2. Create the child's fresh address space.
        let mut child_mapper = self.create_user_mapper();

        // 3. Copy each user page into a private frame and map it in the child.
        for (vaddr, src_phys, flags) in user_pages {
            let dst_frame = self
                .frame_allocator
                .allocate_frame()
                .expect("out of memory: fork user page");

            // Byte-copy the parent's page contents into the child's frame.
            let src_ptr = (offset + src_phys.as_u64()).as_ptr::<u8>();
            let dst_ptr = (offset + dst_frame.start_address().as_u64()).as_mut_ptr::<u8>();
            unsafe {
                core::ptr::copy_nonoverlapping(src_ptr, dst_ptr, 4096);
            }

            // Preserve the parent's page flags exactly (RW/NX/USER bits).
            let page: Page<Size4KiB> = Page::containing_address(VirtAddr::new(vaddr));
            unsafe {
                match child_mapper.map_to(page, dst_frame, flags, &mut self.frame_allocator) {
                    Ok(flush) => flush.flush(),
                    Err(e) => {
                        serial_println!(
                            "memory :: fork: failed to map child page {:#x}: {:?}",
                            vaddr,
                            e
                        );
                    }
                }
            }
        }

        // 4. Register and return the child's CR3.
        self.store_user_mapper(child_mapper)
    }

    /// Store a user mapper (really, its L4 physical address) for later retrieval.
    pub fn store_user_mapper(&mut self, mut mapper: OffsetPageTable<'static>) -> PhysAddr {
        // Extract the physical address of the L4 table from the mapper.
        let l4_ptr = mapper.level_4_table() as *const PageTable;
        let l4_virt = VirtAddr::from_ptr(l4_ptr);
        let l4_phys = PhysAddr::new(l4_virt.as_u64() - self.physical_memory_offset.as_u64());

        self.user_page_tables.push(l4_phys);
        l4_phys
    }

    /// Map `page_count` freshly allocated, zeroed frames into the user address
    /// space identified by `user_cr3`, starting at virtual address `start`.
    ///
    /// Unlike a `with_memory(... mem.mapper ...)` call (which mutates the
    /// *kernel's* page table), this builds an [`OffsetPageTable`] over the
    /// target process's own L4 so the mapping is visible when that process is
    /// scheduled. Used by `sys_mmap` to grow a user heap. Returns `Err(())` if
    /// a frame can't be allocated or a page is already mapped.
    pub fn map_user_region(
        &mut self,
        user_cr3: PhysAddr,
        start: u64,
        page_count: usize,
        writable: bool,
    ) -> Result<(), ()> {
        use x86_64::structures::paging::{FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB};

        let offset = self.physical_memory_offset;
        // SAFETY: `user_cr3` is a valid L4 table physical address recorded when
        // the process's address space was created; the physical-memory window
        // maps it at `offset + phys`.
        let l4: &mut PageTable =
            unsafe { &mut *((offset + user_cr3.as_u64()).as_mut_ptr::<PageTable>()) };
        let mut mapper = unsafe { OffsetPageTable::new(l4, offset) };

        let start_page = Page::<Size4KiB>::containing_address(VirtAddr::new(start));
        let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        if writable {
            flags |= PageTableFlags::WRITABLE;
        }

        for i in 0..page_count {
            let page = start_page + i as u64;
            let frame = self.frame_allocator.allocate_frame().ok_or(())?;
            // Zero the frame so user heap memory starts clean.
            let fptr = (offset + frame.start_address().as_u64()).as_mut_ptr::<u8>();
            unsafe {
                core::ptr::write_bytes(fptr, 0, 4096);
                mapper
                    .map_to(page, frame, flags, &mut self.frame_allocator)
                    .map_err(|_| ())?
                    .flush();
            }
        }
        Ok(())
    }

    /// Translate a physical address to a kernel virtual address.
    pub fn phys_to_virt(&self, phys: PhysAddr) -> VirtAddr {
        VirtAddr::new(self.physical_memory_offset.as_u64() + phys.as_u64())
    }

    /// Allocate a physical frame (convenience wrapper).
    pub fn allocate_frame(&mut self) -> Option<PhysFrame> {
        self.frame_allocator.allocate_frame()
    }

    /// Get a mutable reference to the frame allocator.
    pub fn frame_allocator(&mut self) -> &mut impl FrameAllocator<Size4KiB> {
        &mut self.frame_allocator
    }
}
