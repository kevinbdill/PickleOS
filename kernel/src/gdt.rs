//! Global Descriptor Table (GDT) and Task State Segment (TSS).
//!
//! In 64-bit mode segmentation is mostly vestigial, but a few pieces are still
//! mandatory:
//!   * **Code/data segment descriptors** for kernel (ring 0) and user (ring 3).
//!     The CPU still checks the descriptor privilege level on `iretq`/syscalls.
//!   * **A TSS** holding the *Interrupt Stack Table* (IST) — guaranteed-good
//!     stacks the CPU switches to for critical faults — and the ring-0 stack
//!     pointer (`rsp0`) the CPU loads when transitioning from user to kernel.
//!
//! The double-fault handler runs on a dedicated IST stack so that even if the
//! kernel stack overflows, we still have a valid stack to report the fault
//! instead of triple-faulting (instant reboot).
//!
//! We deliberately use `static mut` (not `lazy_static`) for the TSS because the
//! scheduler must *update* `rsp0` on every context switch (so each task uses
//! its own kernel stack for interrupts/syscalls). A `lazy_static` is immutable,
//! which would make that impossible.

use core::ptr::addr_of;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

/// IST index reserved for the double-fault handler's emergency stack.
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;
/// IST index reserved for a dedicated timer-interrupt stack (available for
/// future use; the timer currently runs on the active kernel stack so that
/// per-task preemptive context switching works correctly).
pub const TIMER_IST_INDEX: u16 = 1;

/// Size of each emergency stack. 20 KiB is plenty for fault reporting.
const STACK_SIZE: usize = 4096 * 5;

// Backing storage for the IST stacks. `static mut` so we can take their
// addresses at runtime; only touched during single-threaded init.
static mut DOUBLE_FAULT_STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];
static mut TIMER_STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];

// The global TSS and GDT. Both are `const`-initialized and then filled in by
// [`init`]. They are mutated only during init (and `rsp0` by the scheduler).
static mut TSS: TaskStateSegment = TaskStateSegment::new();
static mut GDT: GlobalDescriptorTable = GlobalDescriptorTable::new();

/// The segment selectors produced when the GDT is built.
#[derive(Debug, Clone, Copy)]
pub struct Selectors {
    pub code_selector: SegmentSelector,
    pub data_selector: SegmentSelector,
    pub user_code_selector: SegmentSelector,
    pub user_data_selector: SegmentSelector,
    pub tss_selector: SegmentSelector,
}

static mut SELECTORS: Option<Selectors> = None;

/// Build and load the GDT + TSS, reload the segment registers, and load the TSS.
/// After this returns the CPU is using our descriptors and the IST stacks are
/// armed.
pub fn init() {
    use x86_64::instructions::segmentation::{Segment, CS, DS, ES, SS};
    use x86_64::instructions::tables::load_tss;

    unsafe {
        // Point the IST entries at the tops of our emergency stacks (stacks
        // grow downward, so we use the high address).
        let df_top = VirtAddr::from_ptr(addr_of!(DOUBLE_FAULT_STACK)) + STACK_SIZE as u64;
        TSS.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = df_top;
        let timer_top = VirtAddr::from_ptr(addr_of!(TIMER_STACK)) + STACK_SIZE as u64;
        TSS.interrupt_stack_table[TIMER_IST_INDEX as usize] = timer_top;

        // Build the descriptor table. Order matters: user descriptors must have
        // RPL 3, and the kernel code/data come first.
        let code_selector = GDT.add_entry(Descriptor::kernel_code_segment());
        let data_selector = GDT.add_entry(Descriptor::kernel_data_segment());
        let user_data_selector = GDT.add_entry(Descriptor::user_data_segment());
        let user_code_selector = GDT.add_entry(Descriptor::user_code_segment());
        let tss_selector = GDT.add_entry(Descriptor::tss_segment(&*addr_of!(TSS)));

        SELECTORS = Some(Selectors {
            code_selector,
            data_selector,
            user_code_selector,
            user_data_selector,
            tss_selector,
        });

        (*addr_of!(GDT)).load();

        // Reload the segment registers to use our new descriptors, then load
        // the TSS selector into the task register.
        CS::set_reg(code_selector);
        DS::set_reg(data_selector);
        ES::set_reg(data_selector);
        SS::set_reg(data_selector);
        load_tss(tss_selector);
    }
}

/// Expose the selectors (e.g. for entering user mode in `syscall.rs`).
pub fn selectors() -> &'static Selectors {
    // SAFETY: set once during `init`, read-only thereafter.
    unsafe { SELECTORS.as_ref().expect("gdt::init not called") }
}

/// Update the ring-0 stack pointer the CPU loads on a user->kernel transition.
/// The scheduler calls this on every context switch so each task has its own
/// kernel stack for handling interrupts/syscalls taken while it runs.
pub fn set_kernel_stack(stack_top: VirtAddr) {
    // SAFETY: single-CPU; we only ever mutate the rsp0 entry here.
    unsafe {
        TSS.privilege_stack_table[0] = stack_top;
    }
}
