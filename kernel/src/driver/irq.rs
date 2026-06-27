//! IRQ → task bridge: the core primitive for user-space-style device drivers.
//!
//! In a microkernel, device drivers are ordinary tasks, not kernel code. They
//! cannot install interrupt handlers directly — only the kernel's tiny
//! first-level handler runs in interrupt context. Instead, a driver *registers
//! interest* in an IRQ line and then **blocks waiting for IRQ notifications**.
//! When the hardware interrupt fires, the kernel's first-level handler does the
//! absolute minimum (acknowledge the PIC) and calls [`notify_from_isr`], which
//! records a pending notification and wakes the waiting driver task. The driver
//! then runs at normal task priority to service the device.
//!
//! This is the same model seL4 (`seL4_IRQHandler` + notification objects) and
//! Zircon (interrupt objects) use. It keeps interrupt-context work bounded and
//! lets all device logic live in preemptible, capability-confined tasks.
//!
//! ## Interrupt safety
//!
//! Each IRQ line has a lock-free `pending` counter and a `waiter` task id, both
//! atomics. On a single CPU the first-level handler (interrupts disabled) and a
//! driver's [`wait`] (which disables interrupts across its check-and-block) are
//! mutually exclusive, so the wake/sleep handshake has no lost-wakeup race.

use crate::task;
use core::sync::atomic::{AtomicU64, Ordering};

/// Number of PIC IRQ lines we track (IRQ0..IRQ15).
pub const NUM_IRQS: usize = 16;

/// Per-line state for the IRQ bridge.
struct IrqLine {
    /// Number of delivered-but-not-yet-serviced interrupts. Coalesces: a driver
    /// that wakes once for several rapid IRQs still drains them all.
    pending: AtomicU64,
    /// Task id currently blocked in [`wait`] on this line (0 = none).
    waiter: AtomicU64,
    /// Task id that owns (is allowed to handle) this line (0 = unowned). This is
    /// the kernel-side enforcement point backing an `Object::Irq` capability.
    owner: AtomicU64,
    /// Total interrupts ever delivered on this line (diagnostics).
    count: AtomicU64,
}

impl IrqLine {
    const fn new() -> Self {
        IrqLine {
            pending: AtomicU64::new(0),
            waiter: AtomicU64::new(0),
            owner: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }
}

/// Static table of IRQ lines. `AtomicU64` is safe to share without a lock.
#[allow(clippy::declare_interior_mutable_const)]
const INIT: IrqLine = IrqLine::new();
static LINES: [IrqLine; NUM_IRQS] = [INIT; NUM_IRQS];

/// Register the calling task as the owner/handler of `irq`. Returns `false` if
/// the line is already owned by a different task (single-owner policy).
pub fn register(irq: u8, task_id: u64) -> bool {
    let line = match LINES.get(irq as usize) {
        Some(l) => l,
        None => return false,
    };
    // Compare-exchange 0 -> task_id; succeeds only if currently unowned (or
    // re-registering the same owner).
    match line
        .owner
        .compare_exchange(0, task_id, Ordering::AcqRel, Ordering::Acquire)
    {
        Ok(_) => true,
        Err(existing) => existing == task_id,
    }
}

/// Block the calling task until at least one interrupt is pending on `irq`,
/// then consume one and return. The caller must own the line (see [`register`]).
///
/// Returns the number of interrupts that had accumulated (>= 1) so a driver can
/// detect coalescing/overruns.
pub fn wait(irq: u8) -> u64 {
    let line = &LINES[irq as usize];
    let me = task::current_id();
    loop {
        // Critical section: check-and-block must be atomic w.r.t. the ISR.
        x86_64::instructions::interrupts::disable();
        let p = line.pending.load(Ordering::Acquire);
        if p > 0 {
            line.pending.store(0, Ordering::Release);
            x86_64::instructions::interrupts::enable();
            return p;
        }
        // Nothing pending: publish ourselves as the waiter and sleep. The ISR
        // will see `waiter == me` and wake us.
        line.waiter.store(me, Ordering::Release);
        // block_current_and_schedule re-enables interrupts after switching away,
        // so the IRQ can fire while we're blocked.
        task::block_current_and_schedule();
        // Resumed: clear our waiter slot and re-check the counter.
        line.waiter.store(0, Ordering::Release);
    }
}

/// Called from the kernel's first-level interrupt handler when `irq` fires.
/// Records a pending notification and wakes the registered driver task, if any.
/// Must be cheap and non-blocking — it runs in interrupt context.
pub fn notify_from_isr(irq: u8) {
    let line = match LINES.get(irq as usize) {
        Some(l) => l,
        None => return,
    };
    line.pending.fetch_add(1, Ordering::AcqRel);
    line.count.fetch_add(1, Ordering::Relaxed);
    let w = line.waiter.load(Ordering::Acquire);
    if w != 0 {
        // Wake the blocked driver. `unblock` only flips Blocked -> Runnable and
        // is safe to call with interrupts already disabled (ISR context).
        task::unblock(w);
    }
}

/// Returns `(owner_task_id, total_count)` for diagnostics / the shell.
pub fn stats(irq: u8) -> Option<(u64, u64)> {
    LINES.get(irq as usize).map(|l| {
        (
            l.owner.load(Ordering::Relaxed),
            l.count.load(Ordering::Relaxed),
        )
    })
}
