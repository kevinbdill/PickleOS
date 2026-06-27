//! Block device abstraction layer.
//!
//! A *block device* presents storage as a flat array of fixed-size logical
//! blocks (sectors) addressed by LBA (Logical Block Address). This module
//! defines the [`BlockDevice`] trait that all storage backends implement, plus
//! a global registry so higher layers (a future file system) can enumerate and
//! address devices by a stable name (`sata0`, `sata1`, …) instead of caring
//! whether the backend is AHCI, NVMe, virtio-blk, or a RAM disk.
//!
//! ## Design
//! - [`BlockDevice`] is object-safe so we can store `Box<dyn BlockDevice>` in a
//!   registry and hand out `&dyn BlockDevice` references.
//! - Read/write operate on whole blocks. Callers pass a byte buffer sized to a
//!   multiple of [`BlockDevice::block_size`].
//! - The first concrete backend is [`AhciBlockDevice`], a thin wrapper over an
//!   AHCI SATA port that forwards to [`ahci::read_sectors`] / [`ahci::write_sectors`].
//!
//! ## Why a layer at all?
//! It decouples the file system from the driver. When we add NVMe or a RAM disk
//! later, they implement the same trait and the file system code is unchanged.
//! It is also the natural seam at which a real OS would enforce per-device
//! capabilities and scheduling/caching policy.

use crate::driver::ahci;
use crate::serial_println;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use spin::Mutex;

/// Errors a block device operation can return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockError {
    /// The requested LBA range falls outside the device.
    OutOfRange,
    /// The supplied buffer length is not a whole multiple of the block size,
    /// or is too small/large for the requested block count.
    BadBuffer,
    /// The underlying driver reported a hardware/transport error.
    DeviceError(&'static str),
    /// The operation is not supported by this device (e.g. write to read-only).
    Unsupported,
}

/// A fixed-block-size storage device addressed by LBA.
///
/// Implementations must be `Send` so devices can be owned by the global
/// registry and accessed from any task.
pub trait BlockDevice: Send {
    /// Stable, human-readable device name (e.g. `"sata0"`).
    fn name(&self) -> &str;

    /// Size of one logical block in bytes (typically 512).
    fn block_size(&self) -> usize;

    /// Total number of logical blocks on the device.
    fn block_count(&self) -> u64;

    /// Read `count` blocks starting at `lba` into `buf`.
    /// `buf.len()` must equal `count * block_size()`.
    fn read_blocks(&self, lba: u64, count: u32, buf: &mut [u8]) -> Result<(), BlockError>;

    /// Write `count` blocks starting at `lba` from `buf`.
    /// `buf.len()` must equal `count * block_size()`.
    fn write_blocks(&self, lba: u64, count: u32, buf: &[u8]) -> Result<(), BlockError>;

    /// Total capacity in bytes (default: `block_size * block_count`).
    fn capacity_bytes(&self) -> u64 {
        self.block_size() as u64 * self.block_count()
    }
}

/// An AHCI SATA port exposed as a block device.
pub struct AhciBlockDevice {
    name: String,
    port: u8,
    block_size: usize,
    block_count: u64,
}

impl AhciBlockDevice {
    fn validate(&self, lba: u64, count: u32, buf_len: usize) -> Result<(), BlockError> {
        if count == 0 {
            return Err(BlockError::BadBuffer);
        }
        if buf_len != count as usize * self.block_size {
            return Err(BlockError::BadBuffer);
        }
        // lba + count must not exceed the device (checked with overflow safety).
        let end = lba.checked_add(count as u64).ok_or(BlockError::OutOfRange)?;
        if end > self.block_count {
            return Err(BlockError::OutOfRange);
        }
        Ok(())
    }
}

impl BlockDevice for AhciBlockDevice {
    fn name(&self) -> &str {
        &self.name
    }

    fn block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn read_blocks(&self, lba: u64, count: u32, buf: &mut [u8]) -> Result<(), BlockError> {
        self.validate(lba, count, buf.len())?;
        // AHCI sector count is a 16-bit field; split large transfers into chunks.
        let mut done: u64 = 0;
        while done < count as u64 {
            let chunk = core::cmp::min(count as u64 - done, u16::MAX as u64) as u16;
            let off = done as usize * self.block_size;
            let len = chunk as usize * self.block_size;
            ahci::read_sectors(self.port, lba + done, chunk, &mut buf[off..off + len])
                .map_err(BlockError::DeviceError)?;
            done += chunk as u64;
        }
        Ok(())
    }

    fn write_blocks(&self, lba: u64, count: u32, buf: &[u8]) -> Result<(), BlockError> {
        self.validate(lba, count, buf.len())?;
        let mut done: u64 = 0;
        while done < count as u64 {
            let chunk = core::cmp::min(count as u64 - done, u16::MAX as u64) as u16;
            let off = done as usize * self.block_size;
            let len = chunk as usize * self.block_size;
            ahci::write_sectors(self.port, lba + done, chunk, &buf[off..off + len])
                .map_err(BlockError::DeviceError)?;
            done += chunk as u64;
        }
        Ok(())
    }
}

/// Global registry of block devices.
static DEVICES: Mutex<Vec<Box<dyn BlockDevice>>> = Mutex::new(Vec::new());

/// Register a block device. Returns its index in the registry.
pub fn register(dev: Box<dyn BlockDevice>) -> usize {
    let mut devs = DEVICES.lock();
    serial_println!(
        "[block] registered {} ({} blocks x {} bytes = {} MiB)",
        dev.name(),
        dev.block_count(),
        dev.block_size(),
        dev.capacity_bytes() / (1024 * 1024)
    );
    devs.push(dev);
    devs.len() - 1
}

/// Number of registered block devices.
pub fn device_count() -> usize {
    DEVICES.lock().len()
}

/// Summary information about a registered device (for shell listings).
#[derive(Clone)]
pub struct BlockDeviceInfo {
    pub index: usize,
    pub name: String,
    pub block_size: usize,
    pub block_count: u64,
}

/// Return a snapshot of all registered devices.
pub fn list() -> Vec<BlockDeviceInfo> {
    DEVICES
        .lock()
        .iter()
        .enumerate()
        .map(|(index, d)| BlockDeviceInfo {
            index,
            name: String::from(d.name()),
            block_size: d.block_size(),
            block_count: d.block_count(),
        })
        .collect()
}

/// Read blocks from the device at registry index `idx` into a freshly allocated
/// `Vec`. Convenience wrapper used by the shell and (later) the file system.
pub fn read(idx: usize, lba: u64, count: u32) -> Result<Vec<u8>, BlockError> {
    let devs = DEVICES.lock();
    let dev = devs.get(idx).ok_or(BlockError::OutOfRange)?;
    let mut buf = vec![0u8; count as usize * dev.block_size()];
    dev.read_blocks(lba, count, &mut buf)?;
    Ok(buf)
}

/// Write blocks to the device at registry index `idx`.
pub fn write(idx: usize, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
    let devs = DEVICES.lock();
    let dev = devs.get(idx).ok_or(BlockError::OutOfRange)?;
    let bs = dev.block_size();
    if buf.is_empty() || buf.len() % bs != 0 {
        return Err(BlockError::BadBuffer);
    }
    let count = (buf.len() / bs) as u32;
    dev.write_blocks(lba, count, buf)
}

/// Look up a device index by name (e.g. `"sata0"`).
pub fn find_by_name(name: &str) -> Option<usize> {
    DEVICES.lock().iter().position(|d| d.name() == name)
}

/// Non-destructive read/write round-trip self-test against device `idx`.
///
/// Reads the original contents of `lba`, writes a recognizable pattern, reads it
/// back and verifies, then restores the original block. Logs the outcome to the
/// serial console. Returns `Ok(())` if the round-trip matched.
pub fn selftest(idx: usize, lba: u64) -> Result<(), BlockError> {
    let bs = {
        let devs = DEVICES.lock();
        devs.get(idx).ok_or(BlockError::OutOfRange)?.block_size()
    };

    // 1. Save the original block so the test is non-destructive.
    let original = read(idx, lba, 1)?;

    // 2. Build a recognizable pattern (incrementing bytes XOR the LBA).
    let mut pattern = vec![0u8; bs];
    for (i, b) in pattern.iter_mut().enumerate() {
        *b = ((i as u64).wrapping_add(lba) & 0xFF) as u8;
    }

    // 3. Write the pattern, read it back, compare.
    write(idx, lba, &pattern)?;
    let readback = read(idx, lba, 1)?;
    let matched = readback == pattern;

    // 4. Restore the original block regardless of the comparison result.
    let restore = write(idx, lba, &original);

    if !matched {
        serial_println!("[block] SELFTEST FAILED on device {} LBA {}: data mismatch", idx, lba);
        return Err(BlockError::DeviceError("selftest readback mismatch"));
    }
    restore?;
    serial_println!(
        "[block] selftest PASSED on device {} LBA {} ({}-byte round-trip verified + restored)",
        idx, lba, bs
    );
    Ok(())
}

/// Discover all AHCI SATA ports, run IDENTIFY DEVICE on each, and register them
/// as `sataN` block devices. Call after [`ahci::init`] has populated its ports.
pub fn init() {
    let ports = ahci::list_devices();
    let mut n = 0usize;
    for port in ports {
        if port.device_type != ahci::DeviceType::Sata {
            continue;
        }
        let identity = match ahci::identify_device(port.index) {
            Some(id) => id,
            None => {
                serial_println!(
                    "[block] IDENTIFY failed for AHCI port {}; skipping",
                    port.index
                );
                continue;
            }
        };
        let info = ahci::parse_identify(&identity);
        let name = alloc::format!("sata{}", n);
        register(Box::new(AhciBlockDevice {
            name,
            port: port.index,
            block_size: info.sector_size as usize,
            block_count: info.sectors,
        }));
        n += 1;
    }
    serial_println!("[block] initialization complete: {} device(s)", device_count());
}
