# Interrupt-Driven AHCI — Attempted Implementation

## Goal
Replace AHCI's polling-based command completion with interrupt-driven completion using IRQ11.

## What Was Implemented

### 1. PCI Interrupt Line Helper (`pci.rs`)
```rust
pub fn read_interrupt_line(dev: &PciDevice) -> u8
```
Reads offset 0x3C from PCI config space to retrieve the legacy IRQ line assigned to the device (e.g., 11 for AHCI on ICH9).

### 2. IRQ Unmasking Helper (`interrupts.rs`)
```rust
pub fn unmask_irq(irq: u8)
```
Directly manipulates the PIC mask registers (0x21 for primary PIC, 0xA1 for secondary) to enable hardware interrupts on a specific line after the driver has registered for it via `irq::register`.

### 3. AHCI Interrupt Handler (`interrupts.rs`)
```rust
extern "x86-interrupt" fn ahci_interrupt_handler(_stack_frame: InterruptStackFrame)
```
- Sends EOI to the PIC (IRQ11 lives on the secondary PIC)
- Calls `irq::notify_from_isr(11)` to wake the blocked AHCI driver task
- Registered in the IDT at vector `PIC_1_OFFSET + 11` (vector 43)

## The Blocker

When attempting to enable port-level interrupts by writing `0x1` to `PORT_IE` (Port Interrupt Enable register, offset +0x14 from port base), the system **hangs** at the `write_volatile` operation itself.

### Symptoms
- All prior MMIO operations succeed (reads, writes to other registers)
- Capability check passes
- Virtual address mapping succeeds
- The hang occurs at the actual volatile write to `PORT_IE`
- No panic, no error — the system simply freezes

### Debug Trail
1. Verified MMIO capability covers the address range ✓
2. Confirmed the IRQ handler is registered correctly ✓
3. Confirmed `unmask_irq(11)` executes without error ✓
4. Global controller interrupts (GHC.IE) were enabled ✓
5. The write to PORT_IE at `0xfebf1114` specifically hangs

### Hypothesis
The PORT_IE write may trigger an immediate hardware interrupt (e.g., if there's already a pending D2H FIS from port initialization). If that interrupt fires before the driver task is in `irq::wait`, or if there's a subtle lock ordering issue in the IRQ delivery path, the system could deadlock.

## Current State

**AHCI continues to use polling** for command completion (the original implementation). The interrupt infrastructure is in place and tested with other devices (keyboard IRQ1, mouse IRQ12, timer IRQ0), so the general IRQ delivery path is sound.

## Future Work

To complete interrupt-driven AHCI:
1. Investigate the exact sequencing requirements for PORT_IE (must it be written before or after PORT_CMD.ST?)
2. Add defensive checks in `issue_command` to ensure no stray interrupts are pending before blocking
3. Consider using a completion flag instead of direct `irq::wait` to handle spurious interrupts
4. Test on real hardware (QEMU's ICH9 AHCI emulation may have quirks)

## Files Modified

- `kernel/src/driver/pci.rs` — added `read_interrupt_line`
- `kernel/src/interrupts.rs` — added `unmask_irq` helper and `ahci_interrupt_handler`
- `kernel/src/driver/ahci.rs` — *reverted* to polling after PORT_IE write hang

The groundwork for interrupt-driven AHCI is complete; only the AHCI-specific sequencing issue remains.
