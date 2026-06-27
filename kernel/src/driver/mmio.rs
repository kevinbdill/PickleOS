//! Memory-mapped I/O (MMIO) accessor layer with capability checks.
//!
//! Modern devices (AHCI, NVMe, PCIe controllers, GPUs, NICs) expose control
//! registers as physical memory regions. A driver task must hold an
//! [`Object::Mmio`](crate::capability::Object::Mmio) capability to access such
//! a region. This module provides safe, capability-checked read/write primitives
//! at various widths (8/16/32/64 bits).
//!
//! ## Mapping strategy
//! The kernel maintains a virtual mapping of each granted MMIO region in the
//! **kernel's** address space (upper half, shared across all CR3s), configured
//! with `NO_CACHE | WRITE_THROUGH` so CPU caches don't hide device state. The
//! driver task itself does not map the physical frames — it calls these kernel
//! functions via syscalls (or, in the current in-kernel driver tasks, directly).
//!
//! A future enhancement will let true ring-3 drivers map MMIO into their own
//! address space, but for now the kernel mediates every access after checking
//! the capability.

use crate::capability::{Object, Rights};
use crate::memory;
use crate::serial_println;
use crate::task;
use x86_64::structures::paging::{PageTableFlags, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

/// Base of the kernel's MMIO virtual region. We reserve a large chunk of the
/// upper half for device mappings. Each MMIO region gets mapped on-demand when
/// a driver first accesses it (or when the cap is minted).
const MMIO_VIRT_BASE: u64 = 0xFFFF_FF80_0000_0000;
const MMIO_VIRT_SIZE: u64 = 128 * 1024 * 1024; // 128 MiB reserved

/// Depth of the current "kernel-mediated I/O" scope. While greater than zero,
/// trusted in-kernel code (e.g. the ELF loader reading a program off disk on a
/// user task's behalf) is performing device I/O directly, so the per-task MMIO
/// capability check is bypassed: the access is made on the *kernel's* authority,
/// not the currently-scheduled task's. See [`KernelIoGuard`].
static KERNEL_IO_DEPTH: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// RAII guard that marks the enclosing scope as kernel-mediated I/O. MMIO
/// capability checks are skipped for its duration. Use this only around trusted
/// kernel code that legitimately drives hardware on behalf of the kernel (for
/// instance [`crate::elf::load_from_file`]).
pub struct KernelIoGuard {
    _private: (),
}

impl KernelIoGuard {
    /// Enter a kernel-mediated I/O scope.
    pub fn enter() -> Self {
        KERNEL_IO_DEPTH.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        KernelIoGuard { _private: () }
    }
}

impl Drop for KernelIoGuard {
    fn drop(&mut self) {
        KERNEL_IO_DEPTH.fetch_sub(1, core::sync::atomic::Ordering::SeqCst);
    }
}

/// True while at least one [`KernelIoGuard`] is active.
fn in_kernel_io() -> bool {
    KERNEL_IO_DEPTH.load(core::sync::atomic::Ordering::SeqCst) > 0
}

/// Check if the calling task holds an `Object::Mmio` capability covering
/// `[phys_addr, phys_addr+size)` with at least `needed` rights.
fn check_mmio_cap(phys_addr: u64, size: usize, needed: Rights) -> bool {
    // Kernel-mediated I/O (e.g. loading an ELF off disk for exec) runs on the
    // kernel's authority regardless of which user task happens to be current.
    if in_kernel_io() {
        return true;
    }
    let task_id = task::current_id();
    crate::capability::find_object(task_id, needed, |obj| match obj {
        Object::Mmio { phys_base, len } => {
            // The capability must cover the entire access range.
            phys_addr >= *phys_base
                && phys_addr
                    .checked_add(size as u64)
                    .map_or(false, |end| end <= phys_base + len)
        }
        _ => false,
    })
    .is_some()
}

/// Ensure `[phys, phys+size)` is mapped into the kernel's virtual address space
/// with MMIO flags (NO_CACHE + WRITE_THROUGH). Returns the corresponding virtual
/// address, or `None` if mapping fails. This is idempotent: if already mapped,
/// just returns the existing virtual address.
///
/// For simplicity, we use an identity-style offset mapping:
///     virt = MMIO_VIRT_BASE + (phys - 0)
/// so the offset within the region is preserved. A production implementation
/// would track allocated VA ranges in a map and handle overlaps/reuse properly.
fn ensure_mmio_mapped(phys: u64, size: usize) -> Option<VirtAddr> {
    // Round down to page boundary, round size up.
    let page_size = 4096u64;
    let phys_base = phys & !(page_size - 1);
    let offset = phys - phys_base;
    let pages = ((size as u64 + offset + page_size - 1) / page_size) as usize;

    // For now, use a simple offset: virt = MMIO_VIRT_BASE + phys.
    // A real allocator would track used ranges.
    let virt_base = VirtAddr::new(MMIO_VIRT_BASE + phys_base);

    memory::with_memory(|mem| {
        use x86_64::structures::paging::{Mapper, Page};

        let flags = PageTableFlags::PRESENT
            | PageTableFlags::WRITABLE
            | PageTableFlags::NO_CACHE
            | PageTableFlags::WRITE_THROUGH;

        for i in 0..pages {
            let page = Page::<Size4KiB>::containing_address(virt_base + (i as u64 * page_size));
            let frame = PhysFrame::containing_address(PhysAddr::new(phys_base + i as u64 * page_size));

            // If already mapped, skip (idempotent). Otherwise map it.
            if mem.mapper.translate_page(page).is_ok() {
                continue;
            }

            unsafe {
                if let Err(e) = mem.mapper.map_to(page, frame, flags, &mut mem.frame_allocator) {
                    serial_println!("[mmio] failed to map {:?} -> {:?}: {:?}", page, frame, e);
                    return None;
                }
            }
        }
        Some(virt_base + offset)
    })
}

/// Read an 8-bit value from MMIO. Requires `Object::Mmio` cap with `READ` right.
pub fn read_u8(phys_addr: u64) -> Result<u8, &'static str> {
    if !check_mmio_cap(phys_addr, 1, Rights::READ) {
        return Err("no MMIO read capability");
    }
    let virt = ensure_mmio_mapped(phys_addr, 1).ok_or("failed to map MMIO")?;
    Ok(unsafe { core::ptr::read_volatile(virt.as_ptr()) })
}

/// Read a 16-bit value from MMIO. Requires `Object::Mmio` cap with `READ` right.
pub fn read_u16(phys_addr: u64) -> Result<u16, &'static str> {
    if !check_mmio_cap(phys_addr, 2, Rights::READ) {
        return Err("no MMIO read capability");
    }
    let virt = ensure_mmio_mapped(phys_addr, 2).ok_or("failed to map MMIO")?;
    Ok(unsafe { core::ptr::read_volatile(virt.as_ptr()) })
}

/// Read a 32-bit value from MMIO. Requires `Object::Mmio` cap with `READ` right.
pub fn read_u32(phys_addr: u64) -> Result<u32, &'static str> {
    if !check_mmio_cap(phys_addr, 4, Rights::READ) {
        return Err("no MMIO read capability");
    }
    let virt = ensure_mmio_mapped(phys_addr, 4).ok_or("failed to map MMIO")?;
    Ok(unsafe { core::ptr::read_volatile(virt.as_ptr()) })
}

/// Read a 64-bit value from MMIO. Requires `Object::Mmio` cap with `READ` right.
pub fn read_u64(phys_addr: u64) -> Result<u64, &'static str> {
    if !check_mmio_cap(phys_addr, 8, Rights::READ) {
        return Err("no MMIO read capability");
    }
    let virt = ensure_mmio_mapped(phys_addr, 8).ok_or("failed to map MMIO")?;
    Ok(unsafe { core::ptr::read_volatile(virt.as_ptr()) })
}

/// Write an 8-bit value to MMIO. Requires `Object::Mmio` cap with `WRITE` right.
pub fn write_u8(phys_addr: u64, val: u8) -> Result<(), &'static str> {
    if !check_mmio_cap(phys_addr, 1, Rights::WRITE) {
        return Err("no MMIO write capability");
    }
    let virt = ensure_mmio_mapped(phys_addr, 1).ok_or("failed to map MMIO")?;
    unsafe { core::ptr::write_volatile(virt.as_mut_ptr(), val) };
    Ok(())
}

/// Write a 16-bit value to MMIO. Requires `Object::Mmio` cap with `WRITE` right.
pub fn write_u16(phys_addr: u64, val: u16) -> Result<(), &'static str> {
    if !check_mmio_cap(phys_addr, 2, Rights::WRITE) {
        return Err("no MMIO write capability");
    }
    let virt = ensure_mmio_mapped(phys_addr, 2).ok_or("failed to map MMIO")?;
    unsafe { core::ptr::write_volatile(virt.as_mut_ptr(), val) };
    Ok(())
}

/// Write a 32-bit value to MMIO. Requires `Object::Mmio` cap with `WRITE` right.
pub fn write_u32(phys_addr: u64, val: u32) -> Result<(), &'static str> {
    if !check_mmio_cap(phys_addr, 4, Rights::WRITE) {
        return Err("no MMIO write capability");
    }
    let virt = ensure_mmio_mapped(phys_addr, 4).ok_or("failed to map MMIO")?;
    unsafe { core::ptr::write_volatile(virt.as_mut_ptr(), val) };
    Ok(())
}

/// Write a 64-bit value to MMIO. Requires `Object::Mmio` cap with `WRITE` right.
pub fn write_u64(phys_addr: u64, val: u64) -> Result<(), &'static str> {
    if !check_mmio_cap(phys_addr, 8, Rights::WRITE) {
        return Err("no MMIO write capability");
    }
    let virt = ensure_mmio_mapped(phys_addr, 8).ok_or("failed to map MMIO")?;
    unsafe { core::ptr::write_volatile(virt.as_mut_ptr(), val) };
    Ok(())
}
