//! AHCI (Advanced Host Controller Interface) SATA driver.
//!
//! AHCI is the standard interface for SATA controllers on modern x86 systems.
//! It exposes the HBA (Host Bus Adapter) as a memory-mapped register set (the
//! "ABAR" — AHCI Base Address Register, which is BAR5 in PCI config space).
//!
//! ## Architecture
//! - The HBA has up to 32 **ports** (each can connect to one SATA device).
//! - Each port has **command lists**, **FIS receive areas**, and **command tables**
//!   (all in DMA-able physical memory).
//! - Commands (read/write sectors, IDENTIFY DEVICE, etc.) are issued by writing
//!   descriptors into the command list and ringing a doorbell register.
//! - Completions are signaled via interrupts or by polling status registers.
//!
//! ## References
//! - Intel AHCI 1.3.1 specification (public document)
//! - OSDev Wiki: https://wiki.osdev.org/AHCI
//!
//! ## Current status
//! This is a **Phase 3 proof-of-concept**: detection, port initialization, and
//! IDENTIFY DEVICE. Full read/write will come next.

use crate::driver::{dma, mmio, pci};
use crate::serial_println;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use spin::Mutex;
use x86_64::{PhysAddr, VirtAddr};

/// AHCI PCI class code: 01:06:01 (mass storage / SATA / AHCI).
const AHCI_CLASS: u8 = 0x01;
const AHCI_SUBCLASS: u8 = 0x06;
const AHCI_PROG_IF: u8 = 0x01;

/// HBA memory-mapped register offsets (relative to ABAR).
/// See AHCI spec section 3.1 for full register map.
const HBA_CAP: u64 = 0x00;      // Host Capabilities
const HBA_GHC: u64 = 0x04;      // Global Host Control
const HBA_IS: u64 = 0x08;       // Interrupt Status
const HBA_PI: u64 = 0x0C;       // Ports Implemented
const HBA_VS: u64 = 0x10;       // Version

/// Per-port register offsets (port N starts at 0x100 + N*0x80).
const PORT_CLB: u64 = 0x00;     // Command List Base Address (low 32)
const PORT_CLBU: u64 = 0x04;    // Command List Base Address (high 32)
const PORT_FB: u64 = 0x08;      // FIS Base Address (low 32)
const PORT_FBU: u64 = 0x0C;     // FIS Base Address (high 32)
const PORT_IS: u64 = 0x10;      // Interrupt Status
const PORT_IE: u64 = 0x14;      // Interrupt Enable
const PORT_CMD: u64 = 0x18;     // Command and Status
const PORT_TFD: u64 = 0x20;     // Task File Data
const PORT_SIG: u64 = 0x24;     // Signature
const PORT_SSTS: u64 = 0x28;    // SATA Status (SCR0: SStatus)
const PORT_SCTL: u64 = 0x2C;    // SATA Control (SCR2: SControl)
const PORT_SERR: u64 = 0x30;    // SATA Error (SCR1: SError)
const PORT_SACT: u64 = 0x34;    // SATA Active
const PORT_CI: u64 = 0x38;      // Command Issue

/// Port signature values (PORT_SIG register).
const SIG_ATA: u32 = 0x00000101;   // SATA drive
const SIG_ATAPI: u32 = 0xEB140101; // ATAPI device (CD/DVD)
const SIG_SEMB: u32 = 0xC33C0101;  // Enclosure management bridge
const SIG_PM: u32 = 0x96690101;    // Port multiplier

/// Global HBA Control register bits.
const GHC_AHCI_ENABLE: u32 = 1 << 31; // AHCI Enable (AE)
const GHC_INTERRUPT_ENABLE: u32 = 1 << 1; // Interrupt Enable (IE)
const GHC_HBA_RESET: u32 = 1 << 0; // HBA Reset (HR)

/// Port Command and Status register bits.
const PORT_CMD_ST: u32 = 1 << 0;   // Start (enable command processing)
const PORT_CMD_FRE: u32 = 1 << 4;  // FIS Receive Enable
const PORT_CMD_FR: u32 = 1 << 14;  // FIS Receive Running
const PORT_CMD_CR: u32 = 1 << 15;  // Command List Running

/// Port SATA Status register (SCR0) bits.
/// Bits 3:0 = Device Detection (DET): 3 = device present and PHY established.
const SSTS_DET_MASK: u32 = 0xF;
const SSTS_DET_PRESENT: u32 = 0x3;

/// A discovered AHCI controller with its MMIO base.
#[derive(Debug)]
pub struct AhciController {
    pub pci_dev: pci::PciDevice,
    pub abar_phys: PhysAddr,
    pub abar_virt: VirtAddr,
    pub ports_implemented: u32, // bitmap of which ports are usable
    pub num_ports: u8,          // max port count from HBA_CAP
}

/// Per-port state (device type, command lists, etc.).
#[derive(Debug, Clone)]
pub struct AhciPort {
    pub index: u8,
    pub signature: u32,
    pub device_type: DeviceType,
    /// Physical address of the command list (1 KiB, 32 command headers).
    pub clb_phys: u64,
    /// Physical address of the FIS receive area (256 bytes).
    pub fb_phys: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
    None,
    Sata,
    Atapi,
    Semb,
    PortMultiplier,
}

impl DeviceType {
    fn from_signature(sig: u32) -> Self {
        match sig {
            SIG_ATA => DeviceType::Sata,
            SIG_ATAPI => DeviceType::Atapi,
            SIG_SEMB => DeviceType::Semb,
            SIG_PM => DeviceType::PortMultiplier,
            0xFFFFFFFF => DeviceType::None, // uninitialized/not ready
            _ => DeviceType::None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            DeviceType::None => "None/Unknown",
            DeviceType::Sata => "SATA",
            DeviceType::Atapi => "ATAPI",
            DeviceType::Semb => "SEMB",
            DeviceType::PortMultiplier => "Port Multiplier",
        }
    }
}

static CONTROLLER: Mutex<Option<AhciController>> = Mutex::new(None);
static PORTS: Mutex<Vec<AhciPort>> = Mutex::new(Vec::new());

/// Find and initialize the AHCI controller. Call once during boot after PCI enumeration.
pub fn init() {
    // 1. Find the AHCI controller via PCI.
    let dev = match pci::find_device(|d| {
        d.class_code == AHCI_CLASS && d.subclass == AHCI_SUBCLASS && d.prog_if == AHCI_PROG_IF
    }) {
        Some(d) => d,
        None => {
            serial_println!("[ahci] no AHCI controller found (add -device ahci to QEMU)");
            return;
        }
    };

    serial_println!(
        "[ahci] found controller at {:02x}:{:02x}.{} — {:04x}:{:04x}",
        dev.bus,
        dev.device,
        dev.function,
        dev.vendor_id,
        dev.device_id
    );

    // 2. Read BAR5 (ABAR) — the physical base address of the HBA memory region.
    let bar5 = dev.bars[5];
    if bar5 == 0 {
        serial_println!("[ahci] BAR5 is zero; controller not configured");
        return;
    }

    // Bit 0 = 0 means it's a memory BAR (not I/O port). Mask off the low 4 bits
    // (type/prefetch flags) to get the physical address.
    let abar_phys = PhysAddr::new((bar5 & !0xF) as u64);
    serial_println!("[ahci] ABAR (BAR5) at phys {:#x}", abar_phys.as_u64());

    // 3. Mint an Object::Mmio capability for the ABAR region (assume 8 KiB, which
    //    covers the generic host registers + 32 ports). A production driver would
    //    read the actual size from the BAR.
    let abar_size = 8 * 1024;
    let task_id = crate::task::current_id();
    crate::capability::mint(
        task_id,
        crate::capability::Object::Mmio {
            phys_base: abar_phys.as_u64(),
            len: abar_size,
        },
        crate::capability::Rights::READ.union(crate::capability::Rights::WRITE),
    );

    // 4. Map the ABAR into kernel virtual space via the MMIO layer (which will
    //    check our capability). For the first access, the mapping is established.
    //    Subsequent reads/writes reuse it.
    let cap_reg = mmio::read_u32(abar_phys.as_u64()).expect("failed to read HBA_CAP");
    let num_ports = ((cap_reg & 0x1F) + 1) as u8; // bits 4:0 = NP (number of ports - 1)
    let version = mmio::read_u32(abar_phys.as_u64() + HBA_VS).expect("failed to read HBA_VS");
    serial_println!(
        "[ahci] HBA version {}.{}, {} ports max",
        (version >> 16) & 0xFFFF,
        version & 0xFFFF,
        num_ports
    );

    // 5. Perform a global HBA reset to get a clean slate.
    let ghc_addr = abar_phys.as_u64() + HBA_GHC;
    let mut ghc = mmio::read_u32(ghc_addr).expect("failed to read GHC");
    ghc |= GHC_HBA_RESET;
    mmio::write_u32(ghc_addr, ghc).expect("failed to write GHC (reset)");
    // Wait for the reset to complete (HR bit self-clears). Timeout after a few iterations.
    for _ in 0..100 {
        ghc = mmio::read_u32(ghc_addr).expect("failed to read GHC");
        if (ghc & GHC_HBA_RESET) == 0 {
            break;
        }
        crate::task::sleep_yield(); // give the HBA time
    }
    if (ghc & GHC_HBA_RESET) != 0 {
        serial_println!("[ahci] warning: HBA reset did not complete");
    }

    // 6. Enable AHCI mode.
    ghc = mmio::read_u32(ghc_addr).expect("failed to read GHC");
    ghc |= GHC_AHCI_ENABLE;
    mmio::write_u32(ghc_addr, ghc).expect("failed to write GHC (enable AHCI)");

    // 7. Read which ports are implemented (a bitmap).
    let pi = mmio::read_u32(abar_phys.as_u64() + HBA_PI).expect("failed to read PI");
    serial_println!("[ahci] ports implemented: {:#010x}", pi);

    // The ABAR virtual address (the MMIO layer internally computes this as
    // MMIO_VIRT_BASE + phys). For simplicity, we'll just store the phys and
    // recompute virt as needed, or we can expose the internal mapping. For now,
    // assume MMIO_VIRT_BASE + abar_phys.
    let abar_virt = VirtAddr::new(0xFFFF_FF80_0000_0000 + abar_phys.as_u64());

    *CONTROLLER.lock() = Some(AhciController {
        pci_dev: dev,
        abar_phys,
        abar_virt,
        ports_implemented: pi,
        num_ports,
    });

    // 8. Probe each implemented port to see if a device is attached.
    let mut ports = Vec::new();
    for i in 0..32 {
        if (pi & (1 << i)) == 0 {
            continue; // port not implemented
        }
        if let Some(port) = probe_port(abar_phys, i) {
            serial_println!(
                "[ahci] port {} — {:?} device (sig {:#010x})",
                i,
                port.device_type,
                port.signature
            );
            ports.push(port);
        }
    }

    *PORTS.lock() = ports;
    let count = PORTS.lock().len();
    serial_println!("[ahci] initialization complete: {} device(s) detected", count);
}

/// Probe and initialize a single port: check if a device is present, allocate
/// DMA structures, spin up the port, and read its signature.
fn probe_port(abar_phys: PhysAddr, index: u8) -> Option<AhciPort> {
    let port_base = abar_phys.as_u64() + 0x100 + (index as u64) * 0x80;

    // 1. Check initial SATA Status. If no device is electrically present, skip init.
    let ssts = mmio::read_u32(port_base + PORT_SSTS).ok()?;
    let det = ssts & SSTS_DET_MASK;
    if det == 0 {
        // No device attached at all.
        return None;
    }

    serial_println!("[ahci] port {} — initializing (SSTS DET={:#x})", index, det);

    // 2. Stop the port: clear ST (Start) and FRE (FIS Receive Enable) bits.
    //    We must do this before programming the base address registers.
    let cmd_addr = port_base + PORT_CMD;
    let mut cmd = mmio::read_u32(cmd_addr).ok()?;
    cmd &= !(PORT_CMD_ST | PORT_CMD_FRE);
    mmio::write_u32(cmd_addr, cmd).ok()?;

    // Wait for CR (Command List Running) and FR (FIS Receive Running) to clear.
    for _ in 0..100 {
        cmd = mmio::read_u32(cmd_addr).ok()?;
        if (cmd & (PORT_CMD_CR | PORT_CMD_FR)) == 0 {
            break;
        }
        crate::task::sleep_yield();
    }
    if (cmd & (PORT_CMD_CR | PORT_CMD_FR)) != 0 {
        serial_println!("[ahci] port {} — warning: CR/FR did not clear", index);
    }

    // 3. Allocate DMA memory for command list (1 KiB) and FIS receive area (256 bytes).
    let (_clb_virt, clb_phys, _) = dma::alloc_dma(1024).ok_or_else(|| {
        serial_println!("[ahci] port {} — failed to allocate command list", index);
    }).ok()?;
    let (_fb_virt, fb_phys, _) = dma::alloc_dma(256).ok_or_else(|| {
        serial_println!("[ahci] port {} — failed to allocate FIS RX area", index);
    }).ok()?;

    // 4. Program the base address registers (64-bit physical addresses).
    mmio::write_u32(port_base + PORT_CLB, (clb_phys.as_u64() & 0xFFFFFFFF) as u32).ok()?;
    mmio::write_u32(port_base + PORT_CLBU, (clb_phys.as_u64() >> 32) as u32).ok()?;
    mmio::write_u32(port_base + PORT_FB, (fb_phys.as_u64() & 0xFFFFFFFF) as u32).ok()?;
    mmio::write_u32(port_base + PORT_FBU, (fb_phys.as_u64() >> 32) as u32).ok()?;

    // 5. Clear SERR (SATA Error) register by writing 1s to clear bits.
    let serr = mmio::read_u32(port_base + PORT_SERR).ok()?;
    mmio::write_u32(port_base + PORT_SERR, serr).ok()?;

    // 6. Clear interrupt status by writing 1s.
    let is = mmio::read_u32(port_base + PORT_IS).ok()?;
    mmio::write_u32(port_base + PORT_IS, is).ok()?;

    // 7. Spin up the device: set SCTL (SATA Control) DET field to 1 (perform COMRESET).
    //    This initiates the SATA link and causes the device to send its signature FIS.
    let sctl_addr = port_base + PORT_SCTL;
    let mut sctl = mmio::read_u32(sctl_addr).ok()?;
    sctl = (sctl & !0xF) | 0x1; // DET = 1 (initiate COMRESET)
    mmio::write_u32(sctl_addr, sctl).ok()?;

    // Wait a bit for the reset to propagate.
    for _ in 0..10 {
        crate::task::sleep_yield();
    }

    // Clear DET back to 0 (no action, let normal SATA comm resume).
    sctl = sctl & !0xF;
    mmio::write_u32(sctl_addr, sctl).ok()?;

    // 8. Wait for the device to become ready: DET should transition to 3 (device present + PHY up).
    let mut ready = false;
    for _ in 0..200 {
        let ssts_new = mmio::read_u32(port_base + PORT_SSTS).ok()?;
        let det_new = ssts_new & SSTS_DET_MASK;
        if det_new == SSTS_DET_PRESENT {
            ready = true;
            break;
        }
        crate::task::sleep_yield();
    }
    if !ready {
        serial_println!("[ahci] port {} — device did not come ready after COMRESET", index);
        return None;
    }

    // 9. Enable FIS receive and command processing.
    cmd = mmio::read_u32(cmd_addr).ok()?;
    cmd |= PORT_CMD_FRE;
    mmio::write_u32(cmd_addr, cmd).ok()?;

    // Wait for FR to become set.
    for _ in 0..100 {
        cmd = mmio::read_u32(cmd_addr).ok()?;
        if (cmd & PORT_CMD_FR) != 0 {
            break;
        }
        crate::task::sleep_yield();
    }

    cmd = mmio::read_u32(cmd_addr).ok()?;
    cmd |= PORT_CMD_ST;
    mmio::write_u32(cmd_addr, cmd).ok()?;

    // 10. Read the signature register. The device should have sent a D2H Register FIS
    //     after COMRESET, and the HBA will latch the signature into this register.
    let signature = mmio::read_u32(port_base + PORT_SIG).ok()?;
    let device_type = DeviceType::from_signature(signature);

    serial_println!(
        "[ahci] port {} — initialized: {:?} (sig {:#010x})",
        index,
        device_type,
        signature
    );

    Some(AhciPort {
        index,
        signature,
        device_type,
        clb_phys: clb_phys.as_u64(),
        fb_phys: fb_phys.as_u64(),
    })
}

/// List detected AHCI devices (for shell commands / debugging).
pub fn list_devices() -> Vec<AhciPort> {
    PORTS.lock().clone()
}

// ---------------------------------------------------------------------------
// AHCI command submission
// ---------------------------------------------------------------------------

/// Command header structure (32 bytes). Sits in the command list.
#[repr(C, packed)]
struct CommandHeader {
    /// DW0 low 16 bits — flags:
    ///   bits 4:0 = CFL (Command FIS Length in dwords, 2-16)
    ///   bit 5 = A (ATAPI)
    ///   bit 6 = W (Write, 1 = H2D data)
    ///   bit 7 = P (Prefetchable)
    ///   bit 8 = R (Reset), bit 9 = B (BIST), bit 10 = C (Clear busy)
    ///   bits 15:12 = PMP (port multiplier port)
    flags: u16,
    /// DW0 high 16 bits — PRDTL (number of Physical Region Descriptor entries).
    prdtl: u16,
    /// DW1: PRDBC (Physical Region Descriptor Byte Count) — transferred so far.
    prdbc: u32,
    /// DW2: Command Table Base Address (low 32 bits).
    ctba: u32,
    /// DW3: Command Table Base Address (high 32 bits).
    ctbau: u32,
    /// DW4-7: reserved.
    _reserved: [u32; 4],
}

/// FIS (Frame Information Structure) types.
const FIS_TYPE_REG_H2D: u8 = 0x27; // Register FIS - host to device

/// H2D Register FIS structure (20 bytes).
#[repr(C, packed)]
struct FisRegH2D {
    fis_type: u8,     // 0x27
    flags: u8,        // bit 7 = C (command/control), bits 3:0 = port multiplier
    command: u8,      // ATA command register
    feature_lo: u8,   // Features register (7:0)
    lba_lo: u8,       // LBA bits 7:0
    lba_mid: u8,      // LBA bits 15:8
    lba_hi: u8,       // LBA bits 23:16
    device: u8,       // Device register
    lba_lo_exp: u8,   // LBA bits 31:24 (LBA mode exp)
    lba_mid_exp: u8,  // LBA bits 39:32
    lba_hi_exp: u8,   // LBA bits 47:40
    feature_hi: u8,   // Features register (15:8)
    count_lo: u8,     // Count register (7:0)
    count_hi: u8,     // Count register (15:8)
    icc: u8,          // Isochronous Command Completion
    control: u8,      // Control register
    _reserved: [u8; 4],
}

/// ATA command codes.
const ATA_CMD_IDENTIFY: u8 = 0xEC;
const ATA_CMD_READ_DMA_EXT: u8 = 0x25; // 48-bit LBA read
const ATA_CMD_WRITE_DMA_EXT: u8 = 0x35; // 48-bit LBA write
const ATA_CMD_FLUSH_CACHE_EXT: u8 = 0xEA; // flush write cache (48-bit)

/// Default logical sector size used for block transfers. Real sector size is
/// reported by IDENTIFY DEVICE (`DriveInfo::sector_size`), but virtually all
/// SATA disks present 512-byte logical sectors.
pub const SECTOR_SIZE: usize = 512;

/// Bits in PORT_TFD that indicate the device is busy or has data ready.
const TFD_BSY: u32 = 1 << 7; // Busy
const TFD_DRQ: u32 = 1 << 3; // Data Request
const TFD_ERR: u32 = 1 << 0; // Error

/// Transfer direction for a command's data payload.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    /// No data phase (e.g. FLUSH CACHE).
    None,
    /// Device → host (READ, IDENTIFY).
    Read,
    /// Host → device (WRITE).
    Write,
}

/// Build a command in slot 0, issue it, and poll for completion.
///
/// This is the single code path used by IDENTIFY / READ / WRITE. It assembles
/// the command header in the port's command list, builds the command table
/// (H2D Register FIS + one PRD entry) in a freshly allocated DMA buffer, sets
/// the LBA / count / device fields, rings the doorbell (PORT_CI bit 0), and
/// waits for the command to retire.
///
/// `data_phys`/`byte_count` describe the DMA data buffer (may be 0 for
/// `Direction::None`). Returns `Ok(())` on success.
fn issue_command(
    abar_phys: PhysAddr,
    clb_phys: u64,
    port_index: u8,
    command: u8,
    lba: u64,
    sector_count: u16,
    data_phys: u64,
    byte_count: u32,
    dir: Direction,
) -> Result<(), &'static str> {
    let port_base = abar_phys.as_u64() + 0x100 + (port_index as u64) * 0x80;

    // 1. Wait until the port is not busy before submitting.
    if !wait_not_busy(port_base, 1000) {
        return Err("port busy before command");
    }

    // 2. Allocate a command table (256 bytes: FIS + reserved + one PRD).
    //    This is a transient allocation: the caller (identify/read/write) brackets
    //    the whole operation with `dma::mark()`/`dma::reset_to()`, so the command
    //    table is reclaimed as soon as the operation returns.
    let (_ct_virt, ct_phys, _) = dma::alloc_dma(256).ok_or("dma alloc (command table) failed")?;

    // 3. Build the command header (slot 0) in the command list.
    let clb_virt = dma::phys_to_virt(clb_phys);
    let cmd_hdr = unsafe { &mut *(clb_virt.as_mut_ptr::<CommandHeader>()) };
    let prdtl: u16 = if byte_count > 0 { 1 } else { 0 };
    let mut flags: u16 = 5 & 0x1F; // CFL = 5 dwords (20-byte H2D FIS)
    if dir == Direction::Write {
        flags |= 1 << 6; // W bit: host-to-device data
    }
    cmd_hdr.flags = flags;
    cmd_hdr.prdtl = prdtl;
    cmd_hdr.prdbc = 0;
    cmd_hdr.ctba = (ct_phys.as_u64() & 0xFFFFFFFF) as u32;
    cmd_hdr.ctbau = (ct_phys.as_u64() >> 32) as u32;

    // 4. Build the command table (zeroed) and the H2D Register FIS.
    let ct_virt = dma::phys_to_virt(ct_phys.as_u64());
    unsafe {
        core::ptr::write_bytes(ct_virt.as_mut_ptr::<u8>(), 0, 256);
    }
    let fis = unsafe { &mut *(ct_virt.as_mut_ptr::<FisRegH2D>()) };
    fis.fis_type = FIS_TYPE_REG_H2D;
    fis.flags = 0x80; // C bit: this is a command
    fis.command = command;
    // 48-bit LBA, split across the two register banks.
    fis.lba_lo = (lba & 0xFF) as u8;
    fis.lba_mid = ((lba >> 8) & 0xFF) as u8;
    fis.lba_hi = ((lba >> 16) & 0xFF) as u8;
    fis.lba_lo_exp = ((lba >> 24) & 0xFF) as u8;
    fis.lba_mid_exp = ((lba >> 32) & 0xFF) as u8;
    fis.lba_hi_exp = ((lba >> 40) & 0xFF) as u8;
    // device register: bit 6 = LBA mode (required for DMA EXT commands).
    fis.device = if command == ATA_CMD_IDENTIFY { 0 } else { 1 << 6 };
    fis.count_lo = (sector_count & 0xFF) as u8;
    fis.count_hi = ((sector_count >> 8) & 0xFF) as u8;

    // 5. Build the single PRD entry at offset 128, if there is a data phase.
    if byte_count > 0 {
        let prd_virt = ct_virt + 128u64;
        unsafe {
            *(prd_virt.as_mut_ptr::<u32>()) = (data_phys & 0xFFFFFFFF) as u32; // DBA low
            *(prd_virt.as_mut_ptr::<u32>().add(1)) = (data_phys >> 32) as u32; // DBA high
            *(prd_virt.as_mut_ptr::<u32>().add(2)) = 0; // reserved
            // DBC = byte count - 1 (bit 0 must be 1). Bit 31 (I) left clear.
            *(prd_virt.as_mut_ptr::<u32>().add(3)) = (byte_count - 1) | 1;
        }
    }

    // 6. Clear any stale interrupt status, then issue the command (slot 0).
    let is_addr = port_base + PORT_IS;
    let is = mmio::read_u32(is_addr)?;
    mmio::write_u32(is_addr, is)?;

    let ci_addr = port_base + PORT_CI;
    mmio::write_u32(ci_addr, 0x1)?;

    // 7. Poll for completion: PORT_CI bit 0 clears when the command retires.
    let mut completed = false;
    for _ in 0..2000 {
        if (mmio::read_u32(ci_addr)? & 0x1) == 0 {
            completed = true;
            break;
        }
        // Surface a task-file error early instead of spinning to the timeout.
        if (mmio::read_u32(port_base + PORT_TFD)? & TFD_ERR) != 0 {
            break;
        }
        crate::task::sleep_yield();
    }
    if !completed {
        return Err("command timeout");
    }

    // 8. Final task-file error check.
    let tfd = mmio::read_u32(port_base + PORT_TFD)?;
    if (tfd & TFD_ERR) != 0 {
        serial_println!("[ahci] command {:#x} error on port {} (TFD={:#x})", command, port_index, tfd);
        return Err("command failed (TFD error bit set)");
    }

    Ok(())
}

/// Spin until PORT_TFD shows the device is neither BSY nor DRQ, or `tries`
/// iterations elapse. Returns `true` if the port became ready.
fn wait_not_busy(port_base: u64, tries: u32) -> bool {
    for _ in 0..tries {
        match mmio::read_u32(port_base + PORT_TFD) {
            Ok(tfd) if (tfd & (TFD_BSY | TFD_DRQ)) == 0 => return true,
            Ok(_) => crate::task::sleep_yield(),
            Err(_) => return false,
        }
    }
    false
}

/// Resolve the controller's ABAR and a port's command-list physical address.
/// Returns `None` if the controller is uninitialized or the port is unknown.
fn port_context(port_index: u8) -> Option<(PhysAddr, u64)> {
    let abar_phys = CONTROLLER.lock().as_ref()?.abar_phys;
    let clb_phys = PORTS.lock().iter().find(|p| p.index == port_index)?.clb_phys;
    Some((abar_phys, clb_phys))
}

/// Issue an IDENTIFY DEVICE command to the specified port and return the 512-byte
/// identity data buffer. Returns `None` on timeout or error.
pub fn identify_device(port_index: u8) -> Option<[u8; 512]> {
    let (abar_phys, clb_phys) = port_context(port_index)?;

    // Scope all transient DMA (data buffer + command table) to this call so the
    // bump-allocated pool is reclaimed on return regardless of success/failure.
    let mark = dma::mark();
    let result = (|| -> Option<[u8; 512]> {
        // Allocate a 512-byte data buffer in the DMA pool for the identity response.
        let (data_virt, data_phys, _) = dma::alloc_dma(SECTOR_SIZE)?;

        issue_command(
            abar_phys,
            clb_phys,
            port_index,
            ATA_CMD_IDENTIFY,
            0,
            1,
            data_phys.as_u64(),
            SECTOR_SIZE as u32,
            Direction::Read,
        )
        .map_err(|e| serial_println!("[ahci] IDENTIFY DEVICE failed on port {}: {}", port_index, e))
        .ok()?;

        let mut identity = [0u8; 512];
        unsafe {
            core::ptr::copy_nonoverlapping(data_virt.as_ptr::<u8>(), identity.as_mut_ptr(), 512);
        }
        Some(identity)
    })();
    dma::reset_to(mark);
    result
}

/// Read `count` sectors starting at `lba` from `port_index` into `buf`.
///
/// `buf` must be at least `count * SECTOR_SIZE` bytes. Uses ATA READ DMA EXT
/// (48-bit LBA). Returns the number of bytes read on success.
pub fn read_sectors(port_index: u8, lba: u64, count: u16, buf: &mut [u8]) -> Result<usize, &'static str> {
    if count == 0 {
        return Ok(0);
    }
    let bytes = count as usize * SECTOR_SIZE;
    if buf.len() < bytes {
        return Err("buffer too small for requested sector count");
    }
    let (abar_phys, clb_phys) = port_context(port_index).ok_or("unknown AHCI port")?;

    // Scope transient DMA (bounce buffer + command table) so it is reclaimed on
    // return — otherwise every read would leak and the pool would soon exhaust.
    let mark = dma::mark();
    let result = (|| -> Result<usize, &'static str> {
        // Bounce buffer in the DMA pool (device DMAs here; we copy out to `buf`).
        let (data_virt, data_phys, _) =
            dma::alloc_dma(bytes).ok_or("dma alloc (read buffer) failed")?;

        issue_command(
            abar_phys,
            clb_phys,
            port_index,
            ATA_CMD_READ_DMA_EXT,
            lba,
            count,
            data_phys.as_u64(),
            bytes as u32,
            Direction::Read,
        )?;

        unsafe {
            core::ptr::copy_nonoverlapping(data_virt.as_ptr::<u8>(), buf.as_mut_ptr(), bytes);
        }
        Ok(bytes)
    })();
    dma::reset_to(mark);
    result
}

/// Write `count` sectors starting at `lba` to `port_index` from `buf`.
///
/// `buf` must be at least `count * SECTOR_SIZE` bytes. Uses ATA WRITE DMA EXT
/// (48-bit LBA) followed by FLUSH CACHE EXT to ensure durability. Returns the
/// number of bytes written on success.
pub fn write_sectors(port_index: u8, lba: u64, count: u16, buf: &[u8]) -> Result<usize, &'static str> {
    if count == 0 {
        return Ok(0);
    }
    let bytes = count as usize * SECTOR_SIZE;
    if buf.len() < bytes {
        return Err("buffer too small for requested sector count");
    }
    let (abar_phys, clb_phys) = port_context(port_index).ok_or("unknown AHCI port")?;

    // Scope transient DMA (bounce buffer + command tables) so it is reclaimed on
    // return — otherwise every write would leak and the pool would soon exhaust.
    let mark = dma::mark();
    let result = (|| -> Result<usize, &'static str> {
        // Bounce buffer: copy caller data into a DMA-able region, then DMA to device.
        let (data_virt, data_phys, _) =
            dma::alloc_dma(bytes).ok_or("dma alloc (write buffer) failed")?;
        unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), data_virt.as_mut_ptr::<u8>(), bytes);
        }

        issue_command(
            abar_phys,
            clb_phys,
            port_index,
            ATA_CMD_WRITE_DMA_EXT,
            lba,
            count,
            data_phys.as_u64(),
            bytes as u32,
            Direction::Write,
        )?;

        // Flush the device's write cache so data is durable.
        issue_command(
            abar_phys,
            clb_phys,
            port_index,
            ATA_CMD_FLUSH_CACHE_EXT,
            0,
            0,
            0,
            0,
            Direction::None,
        )?;

        Ok(bytes)
    })();
    dma::reset_to(mark);
    result
}

/// Parse the IDENTIFY DEVICE response and return drive information.
#[derive(Debug)]
pub struct DriveInfo {
    pub model: String,
    pub serial: String,
    pub sectors: u64,         // total 48-bit LBA sectors
    pub sector_size: u32,     // bytes per logical sector
}

pub fn parse_identify(identity: &[u8; 512]) -> DriveInfo {
    // IDENTIFY DEVICE data is a 256-word (512-byte) little-endian structure.
    // The buffer is a `[u8; 512]` with alignment 1, so we must NOT cast it to a
    // `*const u16` (that is undefined behavior on unaligned access). Instead read
    // each 16-bit word from its two little-endian bytes.
    let word = |i: usize| -> u16 {
        (identity[i * 2] as u16) | ((identity[i * 2 + 1] as u16) << 8)
    };

    // ATA strings are stored as byte-swapped pairs (high byte of each word first).
    // Model string: words 27–46 (40 bytes).
    let mut model = Vec::new();
    for i in 27..=46 {
        let w = word(i);
        model.push((w >> 8) as u8);
        model.push((w & 0xFF) as u8);
    }
    let model = String::from_utf8_lossy(&model).trim().to_string();

    // Serial: words 10–19 (20 bytes).
    let mut serial = Vec::new();
    for i in 10..=19 {
        let w = word(i);
        serial.push((w >> 8) as u8);
        serial.push((w & 0xFF) as u8);
    }
    let serial = String::from_utf8_lossy(&serial).trim().to_string();

    // Total 48-bit LBA sectors: words 100–103 (quad-word).
    let sectors = (word(100) as u64)
        | ((word(101) as u64) << 16)
        | ((word(102) as u64) << 32)
        | ((word(103) as u64) << 48);

    // Logical sector size: words 117–118 (if bit 12 of word 106 is set, else 512).
    let sector_size = if (word(106) & (1 << 12)) != 0 {
        let sz = (word(117) as u32) | ((word(118) as u32) << 16);
        sz * 2 // value is in words, convert to bytes
    } else {
        512
    };

    DriveInfo {
        model,
        serial,
        sectors,
        sector_size,
    }
}
