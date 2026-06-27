//! PCI bus enumeration and device discovery.
//!
//! The PCI (Peripheral Component Interconnect) bus is how modern x86 systems
//! connect to most hardware: SATA/AHCI controllers, NVMe drives, network cards,
//! GPUs, USB controllers, etc. This module walks the PCI bus(es) to discover
//! devices and read their configuration space.
//!
//! ## Configuration space access
//! Legacy PCI uses **I/O port** access (not MMIO) via two registers:
//!   * 0xCF8 (`CONFIG_ADDRESS`) — selects which device/function/register to read/write.
//!   * 0xCFC (`CONFIG_DATA`) — the 32-bit data read from or written to that location.
//!
//! A configuration address has this bit layout:
//!   [31]     enable bit (must be 1)
//!   [30:24]  reserved (0)
//!   [23:16]  bus number (0–255)
//!   [15:11]  device number (0–31)
//!   [10:8]   function number (0–7)
//!   [7:2]    register offset (0–63, in dwords; bits 1-0 are always 0)
//!
//! ## Device identification
//! Every PCI device has:
//!   * **Vendor ID** (offset 0x00, 16 bits) — manufacturer (e.g. Intel = 0x8086).
//!   * **Device ID** (offset 0x02, 16 bits) — specific model.
//!   * **Class code** (offset 0x08–0x0B) — device type:
//!       - Base class (0x0B)
//!       - Sub-class (0x0A)
//!       - Programming interface (0x09)
//!       - Revision ID (0x08)
//!     For example, AHCI controllers have class 01:06:01 (mass storage / SATA / AHCI).
//!   * **Base Address Registers (BARs)** (offset 0x10–0x24) — MMIO or I/O port ranges
//!     the device uses. BAR5 (offset 0x24) is the AHCI "ABAR" (HBA memory region).
//!
//! ## Usage
//! Call [`init()`] during early boot to scan bus 0 and populate a global device list.
//! Then use [`find_device()`] to locate a specific vendor/device or class code.

use crate::serial_println;
use alloc::vec::Vec;
use spin::Mutex;
use x86_64::instructions::port::Port;

/// PCI configuration space I/O ports (legacy mechanism #1).
const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

/// A discovered PCI device (vendor, device, class, BARs).
#[derive(Debug, Clone, Copy)]
pub struct PciDevice {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_code: u8,    // base class
    pub subclass: u8,
    pub prog_if: u8,       // programming interface
    pub revision: u8,
    pub bars: [u32; 6],    // Base Address Registers 0–5
}

impl PciDevice {
    /// Human-readable class code string (common ones).
    pub fn class_name(&self) -> &'static str {
        match (self.class_code, self.subclass, self.prog_if) {
            (0x01, 0x06, 0x01) => "AHCI SATA controller",
            (0x01, 0x08, 0x02) => "NVMe controller",
            (0x02, 0x00, _) => "Ethernet controller",
            (0x03, 0x00, _) => "VGA-compatible controller",
            (0x0C, 0x03, _) => "USB controller",
            (0x06, 0x00, _) => "Host bridge",
            (0x06, 0x01, _) => "ISA bridge",
            (0x06, 0x04, _) => "PCI-to-PCI bridge",
            _ => "Unknown",
        }
    }
}

/// Global list of discovered devices. Populated by [`init()`].
static DEVICES: Mutex<Vec<PciDevice>> = Mutex::new(Vec::new());

/// Read a 32-bit value from PCI configuration space.
/// SAFETY: Caller must ensure only one thread accesses CONFIG_ADDRESS/DATA at a time.
unsafe fn pci_config_read(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    let address: u32 = 0x8000_0000
        | ((bus as u32) << 16)
        | ((device as u32) << 11)
        | ((function as u32) << 8)
        | ((offset as u32) & 0xFC);

    let mut addr_port = Port::<u32>::new(CONFIG_ADDRESS);
    let mut data_port = Port::<u32>::new(CONFIG_DATA);

    addr_port.write(address);
    data_port.read()
}

/// Check if a device exists at the given bus/device/function by reading vendor ID.
/// Vendor ID 0xFFFF means no device present.
fn device_exists(bus: u8, device: u8, function: u8) -> bool {
    let val = unsafe { pci_config_read(bus, device, function, 0x00) };
    let vendor_id = (val & 0xFFFF) as u16;
    vendor_id != 0xFFFF
}

/// Probe a single PCI function and return its device descriptor if present.
fn probe_function(bus: u8, device: u8, function: u8) -> Option<PciDevice> {
    if !device_exists(bus, device, function) {
        return None;
    }

    // Offset 0x00: vendor (low 16) + device (high 16)
    let val0 = unsafe { pci_config_read(bus, device, function, 0x00) };
    let vendor_id = (val0 & 0xFFFF) as u16;
    let device_id = (val0 >> 16) as u16;

    // Offset 0x08: class code (byte 3), subclass (byte 2), prog IF (byte 1), revision (byte 0)
    let val8 = unsafe { pci_config_read(bus, device, function, 0x08) };
    let revision = (val8 & 0xFF) as u8;
    let prog_if = ((val8 >> 8) & 0xFF) as u8;
    let subclass = ((val8 >> 16) & 0xFF) as u8;
    let class_code = ((val8 >> 24) & 0xFF) as u8;

    // Read all 6 BARs (offsets 0x10, 0x14, 0x18, 0x1C, 0x20, 0x24)
    let mut bars = [0u32; 6];
    for i in 0..6 {
        let offset = 0x10 + (i as u8) * 4;
        bars[i] = unsafe { pci_config_read(bus, device, function, offset) };
    }

    Some(PciDevice {
        bus,
        device,
        function,
        vendor_id,
        device_id,
        class_code,
        subclass,
        prog_if,
        revision,
        bars,
    })
}

/// Enumerate PCI bus 0 and store discovered devices in the global list.
/// For simplicity, we only scan bus 0 (no bridge recursion yet). This is
/// sufficient to find most on-board devices in QEMU and simple hardware.
pub fn init() {
    let mut devices = Vec::new();

    for device in 0..32 {
        for function in 0..8 {
            if let Some(dev) = probe_function(0, device, function) {
                serial_println!(
                    "[pci] {:02x}:{:02x}.{} — {:04x}:{:04x} ({:02x}:{:02x}:{:02x}) {}",
                    dev.bus,
                    dev.device,
                    dev.function,
                    dev.vendor_id,
                    dev.device_id,
                    dev.class_code,
                    dev.subclass,
                    dev.prog_if,
                    dev.class_name()
                );
                devices.push(dev);

                // If function 0 is not a multi-function device, skip other functions.
                if function == 0 {
                    let header_type = unsafe { pci_config_read(0, device, 0, 0x0C) };
                    let is_multi = ((header_type >> 16) & 0x80) != 0;
                    if !is_multi {
                        break;
                    }
                }
            }
        }
    }

    serial_println!("[pci] enumeration complete: {} device(s) found", devices.len());
    *DEVICES.lock() = devices;
}

/// Find the first PCI device matching a predicate (e.g., specific vendor/device
/// or class code). Returns `None` if no match.
pub fn find_device<F: Fn(&PciDevice) -> bool>(pred: F) -> Option<PciDevice> {
    DEVICES.lock().iter().find(|d| pred(d)).copied()
}

/// List all discovered devices (for debugging / shell commands).
pub fn list_devices() -> Vec<PciDevice> {
    DEVICES.lock().clone()
}

/// Read the interrupt line from PCI config space (offset 0x3C). Returns the
/// legacy PIC IRQ line assigned to this device (e.g., 11 for AHCI on ICH9), or
/// 0xFF if not assigned or using MSI. Only meaningful for devices using legacy
/// (INTx) interrupts.
pub fn read_interrupt_line(dev: &PciDevice) -> u8 {
    let val = unsafe { pci_config_read(dev.bus, dev.device, dev.function, 0x3C) };
    (val & 0xFF) as u8
}
