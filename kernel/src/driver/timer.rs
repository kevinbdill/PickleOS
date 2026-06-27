//! Timer "monitor" driver — demonstrates IRQ delivery to a task for a line that
//! the kernel *also* uses internally.
//!
//! IRQ0 (the PIT) drives preemptive scheduling, so unlike the keyboard we do
//! not move its handling out of the kernel. Instead, the kernel's timer handler
//! both ticks the scheduler **and** notifies this driver, proving that an IRQ
//! can fan out to a user-space-style task without disturbing the kernel's own
//! use of the line. This is the same pattern a real driver (e.g. an HPET or
//! per-device timer) would use to receive periodic interrupts.
//!
//! On a headless boot this task is the primary evidence that the whole
//! IRQ → bridge → task path works, since no keyboard is present to fire IRQ1.

use crate::capability::{self, Object, Rights};
use crate::driver::irq;
use crate::serial_println;
use crate::task;

const TIMER_IRQ: u8 = 0;

/// The timer monitor driver task entry point.
pub extern "C" fn timer_driver_task() -> ! {
    let me = task::current_id();
    capability::mint(me, Object::Irq(TIMER_IRQ), Rights::ALL);

    if !irq::register(TIMER_IRQ, me) {
        serial_println!("[timer-drv] ERROR: IRQ{} already owned", TIMER_IRQ);
    }
    serial_println!("[timer-drv] timer monitor online (owns IRQ{})", TIMER_IRQ);

    let mut received: u64 = 0;
    let mut milestones: u64 = 0;
    loop {
        // Each wakeup returns the number of coalesced interrupts since we last
        // ran, so the count stays accurate even though we sleep between IRQs.
        let n = irq::wait(TIMER_IRQ);
        received += n;

        // Log once per ~100 delivered IRQs to prove sustained delivery without
        // flooding the console.
        if received / 100 > milestones {
            milestones = received / 100;
            let (owner, total) = irq::stats(TIMER_IRQ).unwrap_or((0, 0));
            serial_println!(
                "[timer-drv] serviced {} IRQ0 notifications (line total {}, owner task {})",
                received,
                total,
                owner
            );
        }
    }
}
