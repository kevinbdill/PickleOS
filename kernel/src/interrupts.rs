//! Interrupt handling: the Interrupt Descriptor Table (IDT), CPU exception
//! handlers, the legacy 8259 PIC, and the timer + keyboard IRQ handlers.
//!
//! The IDT maps each of the 256 interrupt vectors to a handler. Vectors 0..31
//! are CPU **exceptions** (divide error, page fault, double fault, ...). We
//! remap the two PICs so hardware IRQs land at vectors 32..47 instead of
//! colliding with the exception vectors.
//!
//! The **timer** interrupt is the heartbeat of preemptive multitasking: every
//! tick it calls into the scheduler, which may switch to another task.

use crate::gdt;
use crate::{println, serial_println};
use lazy_static::lazy_static;
use pic8259::ChainedPics;
use spin::Mutex;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
use x86_64::structures::paging::{FrameAllocator, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB};

/// Base vector for the primary PIC after remapping (just past the 32 CPU
/// exception vectors).
pub const PIC_1_OFFSET: u8 = 32;
pub const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

/// The chained primary+secondary 8259 PICs. `unsafe` because wrong offsets can
/// wedge the machine; the offsets above are the conventional safe choice.
pub static PICS: Mutex<ChainedPics> =
    Mutex::new(unsafe { ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) });

/// Symbolic names for the hardware interrupt vectors we handle.
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum InterruptIndex {
    Timer = PIC_1_OFFSET,        // IRQ0 — programmable interval timer (PIT)
    Keyboard,                    // IRQ1 — PS/2 keyboard
    Ahci = PIC_1_OFFSET + 11,    // IRQ11 — AHCI SATA controller (on ICH9)
    Mouse = PIC_1_OFFSET + 12,   // IRQ12 — PS/2 mouse (secondary PIC)
}

impl InterruptIndex {
    fn as_u8(self) -> u8 {
        self as u8
    }
    fn as_usize(self) -> usize {
        usize::from(self.as_u8())
    }
}

lazy_static! {
    /// The one global IDT. Built once, lazily, then loaded by `init_idt`.
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();

        // --- CPU exceptions ------------------------------------------------
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        idt.divide_error.set_handler_fn(divide_error_handler);
        idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
        idt.general_protection_fault.set_handler_fn(general_protection_fault_handler);
        idt.page_fault.set_handler_fn(page_fault_handler);
        // The double-fault handler runs on its own IST stack so a kernel stack
        // overflow doesn't escalate to a triple fault (instant reboot).
        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault_handler)
                .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
        }

        // --- Hardware IRQs -------------------------------------------------
        idt[InterruptIndex::Timer.as_usize()].set_handler_fn(timer_interrupt_handler);
        idt[InterruptIndex::Keyboard.as_usize()].set_handler_fn(keyboard_interrupt_handler);
        idt[InterruptIndex::Ahci.as_usize()].set_handler_fn(ahci_interrupt_handler);
        idt[InterruptIndex::Mouse.as_usize()].set_handler_fn(mouse_interrupt_handler);

        // --- Software syscall vector (int 0x80) ----------------------------
        // We register the assembly `syscall_stub` *by address* (it isn't a
        // normal `x86-interrupt` fn — it manually saves a full register frame).
        // DPL=3 lets ring-3 user code invoke it.
        unsafe {
            idt[0x80]
                .set_handler_addr(x86_64::VirtAddr::new(crate::syscall::syscall_stub as *const () as u64))
                .set_privilege_level(x86_64::PrivilegeLevel::Ring3);
        }

        idt
    };
}

/// Load the IDT into the CPU. After this, faults and IRQs reach our handlers.
pub fn init_idt() {
    IDT.load();
}

// ---------------------------------------------------------------------------
// CPU exception handlers
// ---------------------------------------------------------------------------

extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    println!("EXCEPTION: BREAKPOINT\n{:#?}", stack_frame);
    serial_println!("EXCEPTION: BREAKPOINT @ {:?}", stack_frame.instruction_pointer);
}

extern "x86-interrupt" fn divide_error_handler(stack_frame: InterruptStackFrame) {
    panic!("EXCEPTION: DIVIDE ERROR\n{:#?}", stack_frame);
}

/// True if the trapped frame was executing in ring 3 (user mode). The CPU
/// pushes the interrupted code selector; its low two bits are the RPL.
#[inline]
fn faulted_in_user_mode(stack_frame: &InterruptStackFrame) -> bool {
    (stack_frame.code_segment & 0b11) == 0b11
}

extern "x86-interrupt" fn invalid_opcode_handler(stack_frame: InterruptStackFrame) {
    if faulted_in_user_mode(&stack_frame) {
        // A buggy user program (e.g. one that executes SSE without the kernel
        // having enabled CR4.OSFXSR) must not take the whole system down.
        // Terminate just the offending task and let the scheduler carry on.
        serial_println!(
            "[fault] INVALID OPCODE in user task @ {:#x}; terminating task",
            stack_frame.instruction_pointer.as_u64()
        );
        crate::task::do_exit(132); // 128 + SIGILL(4)
    }
    panic!("EXCEPTION: INVALID OPCODE\n{:#?}", stack_frame);
}

extern "x86-interrupt" fn general_protection_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) {
    if faulted_in_user_mode(&stack_frame) {
        serial_println!(
            "[fault] GENERAL PROTECTION FAULT in user task @ {:#x} (code {:#x}); terminating task",
            stack_frame.instruction_pointer.as_u64(),
            error_code
        );
        crate::task::do_exit(139); // 128 + SIGSEGV(11)
    }
    serial_println!("\n!!! GENERAL PROTECTION FAULT !!!");
    serial_println!("Error code: {:#x}", error_code);
    serial_println!("Stack frame: {:#?}", stack_frame);
    panic!(
        "EXCEPTION: GENERAL PROTECTION FAULT (code {:#x})\n{:#?}",
        error_code, stack_frame
    );
}

extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    use crate::memory::{with_memory, COW_FLAG};
    use x86_64::registers::control::Cr2;
    use x86_64::structures::paging::{PageTable, PageTableFlags, PhysFrame};
    use x86_64::VirtAddr;

    // CR2 holds the faulting virtual address.
    let addr = Cr2::read();

    // --- COW resolution: attempt to service a write to a COW page -----------
    // Conditions:
    //   1. User-mode fault (page was USER_ACCESSIBLE).
    //   2. Error code indicates a WRITE that caused the fault.
    //   3. Error code indicates PAGE_LEVEL_PROTECTION violation (P flag set =
    //      the page was *present* but permissions blocked the access).
    //   4. The leaf PTE has our COW_FLAG set (bit 9) and WRITABLE is clear.
    if error_code.contains(PageFaultErrorCode::USER_MODE)
        && error_code.contains(PageFaultErrorCode::CAUSED_BY_WRITE)
        && error_code.contains(PageFaultErrorCode::PROTECTION_VIOLATION)
    {
        let fault_vaddr = addr.as_u64();

        let resolved = crate::task::scheduler::with(|s| {
            let cur = s.current;
            let cr3 = s.tasks[cur].user_cr3?;
            let offset = with_memory(|m| m.physical_memory_offset);

            // Walk the faulting task's page tables to reach the L1 entry.
            let l4 = unsafe { &mut *((offset + cr3.as_u64()).as_mut_ptr::<PageTable>()) };
            let i4 = ((fault_vaddr >> 39) & 0x1FF) as usize;
            if l4[i4].is_unused() || l4[i4].flags().contains(PageTableFlags::HUGE_PAGE) {
                return None;
            }
            let l3 = unsafe { &mut *((offset + l4[i4].addr().as_u64()).as_mut_ptr::<PageTable>()) };
            let i3 = ((fault_vaddr >> 30) & 0x1FF) as usize;
            if l3[i3].is_unused() || l3[i3].flags().contains(PageTableFlags::HUGE_PAGE) {
                return None;
            }
            let l2 = unsafe { &mut *((offset + l3[i3].addr().as_u64()).as_mut_ptr::<PageTable>()) };
            let i2 = ((fault_vaddr >> 21) & 0x1FF) as usize;
            if l2[i2].is_unused() || l2[i2].flags().contains(PageTableFlags::HUGE_PAGE) {
                return None;
            }
            let l1 = unsafe {
                &mut *((offset + l2[i2].addr().as_u64()).as_mut_ptr::<PageTable>())
            };
            let i1 = ((fault_vaddr >> 12) & 0x1FF) as usize;

            let entry = &l1[i1];
            let flags = entry.flags();

            // Must be: present, user-accessible, NOT writable, and COW flag set.
            if !flags.contains(PageTableFlags::PRESENT)
                || !flags.contains(PageTableFlags::USER_ACCESSIBLE)
                || flags.contains(PageTableFlags::WRITABLE)
                || !flags.contains(COW_FLAG)
            {
                return None;
            }

            // Genuine COW fault: allocate a new frame, copy data, remap writable.
            let old_frame = entry.frame().ok()?;
            let old_phys = old_frame.start_address();

            let new_frame = with_memory(|m| m.frame_allocator.allocate_frame())?;
            let new_phys = new_frame.start_address();

            // Byte-copy the page content (old shared frame -> new private frame).
            let src_ptr = (offset + old_phys.as_u64()).as_ptr::<u8>();
            let dst_ptr = (offset + new_phys.as_u64()).as_mut_ptr::<u8>();
            unsafe {
                core::ptr::copy_nonoverlapping(src_ptr, dst_ptr, 4096);
            }

            // Update the PTE: point to the new frame, mark it writable, clear COW.
            // Preserve all other flags (USER_ACCESSIBLE, NO_EXECUTE, etc.).
            let mut new_flags = flags;
            new_flags.insert(PageTableFlags::WRITABLE);
            new_flags.remove(COW_FLAG);

            l1[i1].set_addr(new_phys, new_flags);

            // Flush this page from the TLB so the new mapping takes effect.
            unsafe {
                x86_64::instructions::tlb::flush(VirtAddr::new(fault_vaddr));
            }

            serial_println!(
                "[cow] resolved COW fault @ {:#x}: old={:#x} -> new={:#x}",
                fault_vaddr,
                old_phys.as_u64(),
                new_phys.as_u64()
            );

            Some(())
        });

        if resolved.is_some() {
            // COW resolved successfully; return from the fault handler.
            // The CPU re-executes the faulting instruction, which now
            // succeeds because the page is writable in this address space.
            return;
        }
        // Fall through: COW resolution did not apply.
    }

    // A fault originating in user mode means a buggy user program dereferenced
    // bad memory. Kill just that task rather than panicking the whole kernel so
    // the rest of the system (and other apps) keep running.
    if error_code.contains(PageFaultErrorCode::USER_MODE)
        || faulted_in_user_mode(&stack_frame)
    {
        serial_println!(
            "[fault] PAGE FAULT in user task @ {:#x} accessing {:#x} (rsp={:#x}, cs={:#x}); terminating task",
            stack_frame.instruction_pointer.as_u64(),
            addr.as_u64(),
            stack_frame.stack_pointer.as_u64(),
            stack_frame.code_segment
        );
        crate::task::do_exit(139); // 128 + SIGSEGV(11)
    }

    panic!("unhandled page fault\n{:#?}", stack_frame);
}

extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    panic!("EXCEPTION: DOUBLE FAULT\n{:#?}", stack_frame);
}

// ---------------------------------------------------------------------------
// Hardware IRQ handlers
// ---------------------------------------------------------------------------

/// Timer (IRQ0). Sends EOI, bumps the global tick counter, then asks the
/// scheduler to (possibly) preempt the current task.
extern "x86-interrupt" fn timer_interrupt_handler(_stack_frame: InterruptStackFrame) {
    // Acknowledge the interrupt *before* a potential context switch, otherwise
    // the PIC won't deliver further timer interrupts after we switch away.
    unsafe {
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::Timer.as_u8());
    }
    // Fan the timer IRQ out to any registered driver task *in addition to*
    // driving the scheduler. This proves a kernel-shared IRQ line can also be
    // delivered to a user-space-style driver without disturbing the kernel's
    // own use of it.
    crate::driver::irq::notify_from_isr(0);
    crate::task::scheduler::on_timer_tick();
}

/// Keyboard (IRQ1). In the PICKLE OS driver model the kernel does **not** read the
/// keyboard here. It only acknowledges the PIC and notifies the user-space-style
/// keyboard driver task, which then reads the PS/2 data port (0x60) through a
/// capability-checked accessor and forwards the scancode to the shell. The byte
/// remains latched in the controller's output buffer until the driver drains
/// it, so nothing is lost while the driver is scheduled.
extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    unsafe {
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::Keyboard.as_u8());
    }
    crate::driver::irq::notify_from_isr(1);
}

/// AHCI SATA controller (IRQ11). The kernel acknowledges the PIC and notifies the
/// AHCI driver task. The driver is blocked in `irq::wait(11)` inside `issue_command`
/// and will wake when this interrupt fires, indicating command completion. IRQ11
/// lives on the secondary PIC.
extern "x86-interrupt" fn ahci_interrupt_handler(_stack_frame: InterruptStackFrame) {
    unsafe {
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::Ahci.as_u8());
    }
    crate::driver::irq::notify_from_isr(11);
}

/// Mouse (IRQ12). IRQ12 lives on the *secondary* PIC, so the End-Of-Interrupt
/// must be sent to both the secondary and the primary (cascade) PIC. As with
/// the keyboard, the kernel does not read the device here — it only acks the
/// PICs and notifies the mouse driver task, which drains the PS/2 data port and
/// decodes movement packets.
extern "x86-interrupt" fn mouse_interrupt_handler(_stack_frame: InterruptStackFrame) {
    unsafe {
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::Mouse.as_u8());
    }
    crate::driver::irq::notify_from_isr(12);
}

/// Unmask a specific IRQ line at the PIC level, allowing hardware interrupts
/// on that line to reach the CPU. Must be called after registering a driver for
/// the IRQ (via `irq::register`) but before the device starts generating interrupts.
pub fn unmask_irq(irq: u8) {
    unsafe {
        let mut pics = PICS.lock();
        // The pic8259 crate doesn't expose a direct unmask method in all versions,
        // so we manually write to the PIC mask registers. The primary PIC controls
        // IRQs 0-7, the secondary controls IRQs 8-15.
        if irq < 8 {
            // Primary PIC: read current mask, clear the bit for this IRQ, write back.
            let mut mask_port = x86_64::instructions::port::Port::<u8>::new(0x21);
            let mask = mask_port.read();
            mask_port.write(mask & !(1 << irq));
        } else if irq < 16 {
            // Secondary PIC: IRQ line is (irq - 8).
            let mut mask_port = x86_64::instructions::port::Port::<u8>::new(0xA1);
            let mask = mask_port.read();
            mask_port.write(mask & !(1 << (irq - 8)));
        }
    }
}

/// Trigger a breakpoint exception (used by the shell `int3` command and tests)
/// to demonstrate that exception handling works without crashing the kernel.
pub fn trigger_breakpoint() {
    x86_64::instructions::interrupts::int3();
}
