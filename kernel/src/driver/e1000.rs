//! Intel E1000 (82540EM) Gigabit Ethernet NIC driver
//!
//! This driver supports the Intel 82540EM network interface controller, which
//! is emulated by QEMU with `-device e1000`. It provides packet transmission
//! and reception via descriptor rings.
//!
//! ## Hardware overview
//! The E1000 is a PCI device (vendor 0x8086, device 0x100E for the 82540EM model).
//! It uses memory-mapped I/O (MMIO) accessed via BAR0. Key registers include:
//!   - CTRL (0x0000): Device control (link up, reset, etc.)
//!   - STATUS (0x0008): Device status
//!   - RCTL/TCTL: RX/TX control registers
//!   - RDBAL/RDBAH, TDBAL/TDBAH: Descriptor ring base addresses
//!   - RDH/RDT, TDH/TDT: Descriptor ring head/tail pointers
//!
//! ## Descriptor rings
//! The driver uses circular descriptor rings for both RX and TX. Each descriptor
//! points to a DMA buffer. The hardware owns descriptors between HEAD and TAIL;
//! the driver updates TAIL to hand buffers to the hardware.

use crate::driver::{dma, mmio, pci};
use crate::serial_println;
use alloc::vec::Vec;
use spin::Mutex;

/// E1000 PCI vendor/device IDs
const E1000_VENDOR: u16 = 0x8086;
const E1000_DEVICE_82540EM: u16 = 0x100E;

/// E1000 register offsets (from BAR0)
const REG_CTRL: u32 = 0x0000;
const REG_STATUS: u32 = 0x0008;
const REG_EEPROM: u32 = 0x0014;
const REG_CTRL_EXT: u32 = 0x0018;
const REG_ICR: u32 = 0x00C0; // Interrupt Cause Read
const REG_IMS: u32 = 0x00D0; // Interrupt Mask Set
const REG_IMC: u32 = 0x00D8; // Interrupt Mask Clear
const REG_RCTL: u32 = 0x0100; // RX Control
const REG_TCTL: u32 = 0x0400; // TX Control
const REG_TIPG: u32 = 0x0410; // TX Inter-Packet Gap
const REG_RDBAL: u32 = 0x2800; // RX Descriptor Base Address Low
const REG_RDBAH: u32 = 0x2804; // RX Descriptor Base Address High
const REG_RDLEN: u32 = 0x2808; // RX Descriptor Length
const REG_RDH: u32 = 0x2810; // RX Descriptor Head
const REG_RDT: u32 = 0x2818; // RX Descriptor Tail
const REG_TDBAL: u32 = 0x3800; // TX Descriptor Base Address Low
const REG_TDBAH: u32 = 0x3804; // TX Descriptor Base Address High
const REG_TDLEN: u32 = 0x3808; // TX Descriptor Length
const REG_TDH: u32 = 0x3810; // TX Descriptor Head
const REG_TDT: u32 = 0x3818; // TX Descriptor Tail
const REG_MTA: u32 = 0x5200; // Multicast Table Array

/// CTRL register bits
const CTRL_RST: u32 = 1 << 26; // Device Reset
const CTRL_SLU: u32 = 1 << 6;  // Set Link Up

/// RCTL register bits
const RCTL_EN: u32 = 1 << 1;     // Receiver Enable
const RCTL_SBP: u32 = 1 << 2;    // Store Bad Packets
const RCTL_UPE: u32 = 1 << 3;    // Unicast Promiscuous Enable
const RCTL_MPE: u32 = 1 << 4;    // Multicast Promiscuous Enable
const RCTL_BAM: u32 = 1 << 15;   // Broadcast Accept Mode
const RCTL_BSIZE_2048: u32 = 0;  // Buffer Size = 2048 bytes
const RCTL_SECRC: u32 = 1 << 26; // Strip Ethernet CRC

/// TCTL register bits
const TCTL_EN: u32 = 1 << 1;    // Transmit Enable
const TCTL_PSP: u32 = 1 << 3;   // Pad Short Packets
const TCTL_CT_SHIFT: u32 = 4;   // Collision Threshold
const TCTL_COLD_SHIFT: u32 = 12; // Collision Distance

/// RX descriptor status bits
const RX_DESC_DD: u8 = 1 << 0; // Descriptor Done
const RX_DESC_EOP: u8 = 1 << 1; // End of Packet

/// TX descriptor command bits
const TX_DESC_CMD_EOP: u8 = 1 << 0; // End of Packet
const TX_DESC_CMD_RS: u8 = 1 << 3;  // Report Status

/// TX descriptor status bits
const TX_DESC_STA_DD: u8 = 1 << 0; // Descriptor Done

/// Number of RX/TX descriptors (must be multiple of 8)
const NUM_RX_DESC: usize = 32;
const NUM_TX_DESC: usize = 16;

/// RX buffer size (2048 bytes per packet)
const RX_BUFFER_SIZE: usize = 2048;

/// RX descriptor (legacy format)
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy)]
struct RxDesc {
    addr: u64,      // Physical address of buffer
    length: u16,    // Packet length
    checksum: u16,  // Packet checksum
    status: u8,     // Descriptor status
    errors: u8,     // Errors
    special: u16,   // VLAN tag, etc.
}

/// TX descriptor (legacy format)
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy)]
struct TxDesc {
    addr: u64,      // Physical address of buffer
    length: u16,    // Packet length
    cso: u8,        // Checksum offset
    cmd: u8,        // Command
    status: u8,     // Status
    css: u8,        // Checksum start
    special: u16,   // VLAN tag, etc.
}

/// E1000 device state
pub struct E1000 {
    bar0: u64,              // MMIO base address
    mac: [u8; 6],           // MAC address
    rx_descs: u64,          // Physical address of RX descriptor ring
    rx_buffers: Vec<u64>,   // Physical addresses of RX buffers
    rx_tail: usize,         // Current RX tail index
    tx_descs: u64,          // Physical address of TX descriptor ring
    tx_buffers: Vec<u64>,   // Physical addresses of TX buffers
    tx_tail: usize,         // Current TX tail index
}

impl E1000 {
    /// Initialize the E1000 device
    pub fn new(pci_dev: &pci::PciDevice) -> Result<Self, &'static str> {
        serial_println!("e1000: initializing device at {}:{}:{}", 
            pci_dev.bus, pci_dev.device, pci_dev.function);
        
        // Get BAR0 (MMIO base address)
        let bar0 = pci_dev.bars[0] as u64 & !0xF; // Mask off lower bits
        if bar0 == 0 {
            return Err("E1000 BAR0 not configured");
        }
        serial_println!("e1000: BAR0 = 0x{:x}", bar0);
        
        // Read MAC address from EEPROM
        let mac = Self::read_mac_address(bar0);
        serial_println!("e1000: MAC address = {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
        
        // Allocate RX descriptor ring (aligned to 16 bytes)
        let (rx_descs_virt, rx_descs_phys, _) = dma::alloc_dma(NUM_RX_DESC * core::mem::size_of::<RxDesc>())
            .ok_or("Failed to allocate RX descriptor ring")?;
        let rx_descs = rx_descs_phys.as_u64();
        
        // Allocate RX buffers
        let mut rx_buffers = Vec::new();
        for _ in 0..NUM_RX_DESC {
            let (_, buf_phys, _) = dma::alloc_dma(RX_BUFFER_SIZE)
                .ok_or("Failed to allocate RX buffer")?;
            rx_buffers.push(buf_phys.as_u64());
        }
        
        // Initialize RX descriptors
        let rx_desc_slice = unsafe {
            core::slice::from_raw_parts_mut(rx_descs_virt.as_mut_ptr::<RxDesc>(), NUM_RX_DESC)
        };
        for (i, desc) in rx_desc_slice.iter_mut().enumerate() {
            desc.addr = rx_buffers[i];
            desc.status = 0;
        }
        
        // Allocate TX descriptor ring
        let (tx_descs_virt, tx_descs_phys, _) = dma::alloc_dma(NUM_TX_DESC * core::mem::size_of::<TxDesc>())
            .ok_or("Failed to allocate TX descriptor ring")?;
        let tx_descs = tx_descs_phys.as_u64();
        
        // Allocate TX buffers
        let mut tx_buffers = Vec::new();
        for _ in 0..NUM_TX_DESC {
            let (_, buf_phys, _) = dma::alloc_dma(RX_BUFFER_SIZE)
                .ok_or("Failed to allocate TX buffer")?;
            tx_buffers.push(buf_phys.as_u64());
        }
        
        // Initialize TX descriptors
        let tx_desc_slice = unsafe {
            core::slice::from_raw_parts_mut(tx_descs_virt.as_mut_ptr::<TxDesc>(), NUM_TX_DESC)
        };
        for (i, desc) in tx_desc_slice.iter_mut().enumerate() {
            desc.addr = tx_buffers[i];
            desc.status = TX_DESC_STA_DD; // Mark as available
        }
        
        let mut dev = E1000 {
            bar0,
            mac,
            rx_descs,
            rx_buffers,
            rx_tail: 0,
            tx_descs,
            tx_buffers,
            tx_tail: 0,
        };
        
        // Initialize hardware
        dev.hw_init()?;
        
        Ok(dev)
    }
    
    /// Read MAC address from EEPROM
    fn read_mac_address(bar0: u64) -> [u8; 6] {
        // Try reading from EEPROM
        let mac_low = Self::read_eeprom_word(bar0, 0);
        let mac_mid = Self::read_eeprom_word(bar0, 1);
        let mac_high = Self::read_eeprom_word(bar0, 2);
        
        [
            (mac_low & 0xFF) as u8,
            ((mac_low >> 8) & 0xFF) as u8,
            (mac_mid & 0xFF) as u8,
            ((mac_mid >> 8) & 0xFF) as u8,
            (mac_high & 0xFF) as u8,
            ((mac_high >> 8) & 0xFF) as u8,
        ]
    }
    
    /// Read a word from EEPROM
    fn read_eeprom_word(bar0: u64, offset: u8) -> u16 {
        // Start EEPROM read
        let addr = ((offset as u32) << 8) | 1;
        Self::mmio_write32_static(bar0, REG_EEPROM, addr);
        
        // Wait for read to complete (bit 4 = done)
        for _ in 0..1000 {
            let val = Self::mmio_read32_static(bar0, REG_EEPROM);
            if val & (1 << 4) != 0 {
                return ((val >> 16) & 0xFFFF) as u16;
            }
        }
        
        0 // Timeout
    }
    
    /// Initialize hardware registers
    fn hw_init(&mut self) -> Result<(), &'static str> {
        // Reset the device
        self.mmio_write32(REG_CTRL, CTRL_RST);
        
        // Busy-wait for reset (no tasks running yet during init)
        for _ in 0..100000 {
            core::hint::spin_loop();
        }
        
        // Disable interrupts
        self.mmio_write32(REG_IMC, 0xFFFFFFFF);
        self.mmio_read32(REG_ICR); // Clear pending interrupts
        
        // Set link up
        let mut ctrl = self.mmio_read32(REG_CTRL);
        ctrl |= CTRL_SLU;
        self.mmio_write32(REG_CTRL, ctrl);
        
        // Clear multicast table array
        for i in 0..128 {
            self.mmio_write32(REG_MTA + i * 4, 0);
        }
        
        // Setup RX
        self.mmio_write32(REG_RDBAH, (self.rx_descs >> 32) as u32);
        self.mmio_write32(REG_RDBAL, self.rx_descs as u32);
        self.mmio_write32(REG_RDLEN, (NUM_RX_DESC * core::mem::size_of::<RxDesc>()) as u32);
        self.mmio_write32(REG_RDH, 0);
        self.mmio_write32(REG_RDT, (NUM_RX_DESC - 1) as u32);
        
        // Enable RX
        let rctl = RCTL_EN | RCTL_SBP | RCTL_UPE | RCTL_MPE | 
                   RCTL_BAM | RCTL_BSIZE_2048 | RCTL_SECRC;
        self.mmio_write32(REG_RCTL, rctl);
        
        // Setup TX
        self.mmio_write32(REG_TDBAH, (self.tx_descs >> 32) as u32);
        self.mmio_write32(REG_TDBAL, self.tx_descs as u32);
        self.mmio_write32(REG_TDLEN, (NUM_TX_DESC * core::mem::size_of::<TxDesc>()) as u32);
        self.mmio_write32(REG_TDH, 0);
        self.mmio_write32(REG_TDT, 0);
        
        // Set TX IPG (Inter-Packet Gap) for Gigabit Ethernet
        self.mmio_write32(REG_TIPG, 0x00702008);
        
        // Enable TX
        let tctl = TCTL_EN | TCTL_PSP | 
                   (0x0F << TCTL_CT_SHIFT) |  // Collision threshold
                   (0x40 << TCTL_COLD_SHIFT); // Collision distance
        self.mmio_write32(REG_TCTL, tctl);
        
        // Check link status
        let status = self.mmio_read32(REG_STATUS);
        let link_up = (status & 0x02) != 0;  // Bit 1 = link up
        serial_println!("e1000: hardware initialized, link_up={}", link_up);
        Ok(())
    }
    
    /// Transmit a packet
    pub fn transmit(&mut self, data: &[u8]) -> Result<(), &'static str> {
        if data.len() > RX_BUFFER_SIZE {
            return Err("Packet too large");
        }
        
        // Get current TX tail
        let tail = self.tx_tail;
        
        // Check if descriptor is available
        let tx_desc_virt = dma::phys_to_virt(self.tx_descs);
        let tx_desc_slice = unsafe {
            core::slice::from_raw_parts_mut(tx_desc_virt.as_mut_ptr::<TxDesc>(), NUM_TX_DESC)
        };
        
        if tx_desc_slice[tail].status & TX_DESC_STA_DD == 0 {
            return Err("TX ring full");
        }
        
        // Copy packet to TX buffer
        let buf_virt = dma::phys_to_virt(self.tx_buffers[tail]);
        let buf_slice = unsafe {
            core::slice::from_raw_parts_mut(buf_virt.as_mut_ptr::<u8>(), RX_BUFFER_SIZE)
        };
        buf_slice[..data.len()].copy_from_slice(data);
        
        // Setup descriptor
        tx_desc_slice[tail].length = data.len() as u16;
        tx_desc_slice[tail].cmd = TX_DESC_CMD_EOP | TX_DESC_CMD_RS;
        tx_desc_slice[tail].status = 0; // Clear DD bit
        
        // Update tail pointer
        let next_tail = (tail + 1) % NUM_TX_DESC;
        self.tx_tail = next_tail;
        self.mmio_write32(REG_TDT, next_tail as u32);
        
        Ok(())
    }
    
    /// Receive a packet if available
    pub fn receive(&mut self) -> Option<Vec<u8>> {
        // Calculate next RX index to check
        let next_rx = (self.rx_tail + 1) % NUM_RX_DESC;
        
        // Check if descriptor has data
        let rx_desc_virt = dma::phys_to_virt(self.rx_descs);
        let rx_desc_slice = unsafe {
            core::slice::from_raw_parts_mut(rx_desc_virt.as_mut_ptr::<RxDesc>(), NUM_RX_DESC)
        };
        
        if rx_desc_slice[next_rx].status & RX_DESC_DD == 0 {
            return None; // No packet
        }
        
        // Copy packet data — clamp hardware-reported length to our buffer size
        // to prevent an attacker-controlled NIC from causing an OOB read.
        let raw_len = rx_desc_slice[next_rx].length as usize;
        let len = raw_len.min(RX_BUFFER_SIZE);
        if raw_len > RX_BUFFER_SIZE {
            serial_println!("[e1000] RX packet length {} > buffer {}, clamped", raw_len, RX_BUFFER_SIZE);
        }
        let buf_virt = dma::phys_to_virt(self.rx_buffers[next_rx]);
        let buf_slice = unsafe {
            core::slice::from_raw_parts(buf_virt.as_ptr::<u8>(), len)
        };
        let mut packet = Vec::with_capacity(len);
        packet.extend_from_slice(buf_slice);
        
        // Clear descriptor status and hand back to hardware
        rx_desc_slice[next_rx].status = 0;
        self.rx_tail = next_rx;
        self.mmio_write32(REG_RDT, next_rx as u32);
        
        Some(packet)
    }
    
    /// Read 32-bit MMIO register (instance method)
    fn mmio_read32(&self, offset: u32) -> u32 {
        mmio::read_u32(self.bar0 + offset as u64).unwrap_or(0)
    }
    
    /// Write 32-bit MMIO register (instance method)
    fn mmio_write32(&self, offset: u32, value: u32) {
        let _ = mmio::write_u32(self.bar0 + offset as u64, value);
    }
    
    /// Read 32-bit MMIO register (static helper for initialization)
    fn mmio_read32_static(bar0: u64, offset: u32) -> u32 {
        mmio::read_u32(bar0 + offset as u64).unwrap_or(0)
    }
    
    /// Write 32-bit MMIO register (static helper for initialization)
    fn mmio_write32_static(bar0: u64, offset: u32, value: u32) {
        let _ = mmio::write_u32(bar0 + offset as u64, value);
    }
    
    /// Get MAC address
    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }
}

/// Global E1000 device instance
static DEVICE: Mutex<Option<E1000>> = Mutex::new(None);

/// Initialize E1000 driver
pub fn init() -> Result<(), &'static str> {
    serial_println!("e1000: searching for device...");
    
    // Find E1000 device on PCI bus
    let pci_dev = pci::find_device(|d| {
        d.vendor_id == E1000_VENDOR && d.device_id == E1000_DEVICE_82540EM
    }).ok_or("E1000 device not found")?;
    
    serial_println!("e1000: found device - {}", pci_dev.class_name());
    
    // Create device instance with kernel I/O guard (init happens before tasks exist)
    let _guard = mmio::KernelIoGuard::enter();
    let device = E1000::new(&pci_dev)?;
    drop(_guard);
    
    *DEVICE.lock() = Some(device);
    
    serial_println!("e1000: driver initialized");
    Ok(())
}

/// Transmit a packet through the E1000
pub fn transmit(data: &[u8]) -> Result<(), &'static str> {
    let _guard = mmio::KernelIoGuard::enter();
    let mut dev = DEVICE.lock();
    if let Some(ref mut d) = *dev {
        d.transmit(data)
    } else {
        Err("E1000 not initialized")
    }
}

/// Receive a packet from the E1000
pub fn receive() -> Option<Vec<u8>> {
    let _guard = mmio::KernelIoGuard::enter();
    let mut dev = DEVICE.lock();
    if let Some(ref mut d) = *dev {
        d.receive()
    } else {
        None
    }
}

/// Get MAC address
pub fn mac_address() -> Option<[u8; 6]> {
    let dev = DEVICE.lock();
    dev.as_ref().map(|d| d.mac_address())
}
