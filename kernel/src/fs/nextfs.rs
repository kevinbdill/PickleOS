//! NextFS implementation — on-disk structures, allocation, and file operations.

use crate::driver::block::{self, BlockDevice, BlockError};
use crate::serial_println;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use spin::Mutex;

/// NextFS magic number (ASCII "NXFS" in little-endian).
const MAGIC: u32 = 0x5346584E;

/// Block size in bytes. Set to 512 to match standard SATA sector size.
/// A production filesystem would make this dynamic based on the device.
pub const BLOCK_SIZE: usize = 512;

/// Number of direct block pointers in each inode.
///
/// Reduced from 12 to 9 to make room for permission/ownership/timestamp
/// metadata while keeping the on-disk inode at exactly 64 bytes (8 inodes per
/// 512-byte block). Max file size is therefore `(9 + 128) * 512 ≈ 68 KiB`.
const DIRECT_PTRS: usize = 9;

/// Default permission bits for newly created files (`rw-r--r--`).
pub const DEFAULT_FILE_MODE: u16 = 0o644;
/// Default permission bits for newly created directories (`rwxr-xr-x`).
pub const DEFAULT_DIR_MODE: u16 = 0o755;

/// Permission bit masks (POSIX-style, lower 9 bits of `mode`).
pub const S_IRUSR: u16 = 0o400; // owner read
pub const S_IWUSR: u16 = 0o200; // owner write
pub const S_IXUSR: u16 = 0o100; // owner execute
pub const S_IRGRP: u16 = 0o040; // group read
pub const S_IWGRP: u16 = 0o020; // group write
pub const S_IXGRP: u16 = 0o010; // group execute
pub const S_IROTH: u16 = 0o004; // other read
pub const S_IWOTH: u16 = 0o002; // other write
pub const S_IXOTH: u16 = 0o001; // other execute

/// Access-check request bits used by [`NextFS::check_permission`].
pub const MAY_READ: u16 = 0o4;
pub const MAY_WRITE: u16 = 0o2;
pub const MAY_EXEC: u16 = 0o1;

/// Current time as a 32-bit timer-tick count (NextFS has no RTC yet, so it
/// uses the monotonic scheduler tick as a coarse "modification time").
fn now_ticks() -> u32 {
    crate::task::scheduler::ticks() as u32
}

/// Inode 0 is reserved (means "no inode" in directory entries).
const RESERVED_INODE: u32 = 0;

/// Root directory is always inode 1.
pub const ROOT_INODE: u32 = 1;

/// Maximum file name length (null-terminated, so 59 usable chars).
pub const MAX_NAME_LEN: usize = 60;

// ---------------------------------------------------------------------------
// On-disk structures
// ---------------------------------------------------------------------------

/// Superblock: always at block 0.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Superblock {
    magic: u32,            // 0x5346584E ("NXFS")
    block_size: u32,       // 4096
    total_blocks: u32,     // total block count on device
    inode_count: u32,      // number of inode slots
    free_blocks: u32,      // count of unallocated blocks
    free_inodes: u32,      // count of unallocated inodes
    bitmap_start: u32,     // first block of the free-block bitmap
    inode_table_start: u32,// first block of the inode table
    data_start: u32,       // first data block
    _reserved: [u8; 472],  // pad to 512 bytes
}

impl Superblock {
    /// Size of one superblock in bytes (one full block).
    const SIZE: usize = BLOCK_SIZE;

    /// Serialize to a byte buffer (block 0).
    fn to_bytes(&self) -> [u8; BLOCK_SIZE] {
        let mut buf = [0u8; BLOCK_SIZE];
        unsafe {
            core::ptr::copy_nonoverlapping(
                self as *const _ as *const u8,
                buf.as_mut_ptr(),
                core::mem::size_of::<Superblock>(),
            );
        }
        buf
    }

    /// Deserialize from a byte buffer (block 0).
    fn from_bytes(buf: &[u8; BLOCK_SIZE]) -> Self {
        let mut sb = Superblock {
            magic: 0,
            block_size: 0,
            total_blocks: 0,
            inode_count: 0,
            free_blocks: 0,
            free_inodes: 0,
            bitmap_start: 0,
            inode_table_start: 0,
            data_start: 0,
            _reserved: [0; 472],
        };
        unsafe {
            core::ptr::copy_nonoverlapping(
                buf.as_ptr(),
                &mut sb as *mut _ as *mut u8,
                core::mem::size_of::<Superblock>(),
            );
        }
        sb
    }

    /// Check if the magic number is valid.
    fn is_valid(&self) -> bool {
        self.magic == MAGIC && self.block_size == BLOCK_SIZE as u32
    }
}

/// Inode: fixed 64-byte structure (8 inodes per 512-byte block).
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Inode {
    pub inode_type: u8,              // @0:  0 = free, 1 = file, 2 = directory
    pub _pad: u8,                    // @1:  reserved/alignment
    pub mode: u16,                   // @2:  permission bits (lower 9 = rwx ugo)
    pub size: u64,                   // @4:  file size in bytes
    pub uid: u16,                    // @12: owner user id
    pub gid: u16,                    // @14: owner group id
    pub mtime: u32,                  // @16: modification time (timer ticks)
    pub direct: [u32; DIRECT_PTRS],  // @20: direct block pointers (9 × 4 = 36 bytes)
    pub indirect: u32,               // @56: single indirect block pointer
    pub _reserved: [u8; 4],          // @60: pad to 64 bytes total
}

impl Inode {
    const SIZE: usize = 64;

    fn new_file() -> Self {
        Inode {
            inode_type: 1,
            _pad: 0,
            mode: DEFAULT_FILE_MODE,
            size: 0,
            uid: 0,
            gid: 0,
            mtime: 0,
            direct: [0; DIRECT_PTRS],
            indirect: 0,
            _reserved: [0; 4],
        }
    }

    fn new_dir() -> Self {
        Inode {
            inode_type: 2,
            _pad: 0,
            mode: DEFAULT_DIR_MODE,
            size: 0,
            uid: 0,
            gid: 0,
            mtime: 0,
            direct: [0; DIRECT_PTRS],
            indirect: 0,
            _reserved: [0; 4],
        }
    }

    fn is_free(&self) -> bool {
        self.inode_type == 0
    }

    fn is_file(&self) -> bool {
        self.inode_type == 1
    }

    fn is_dir(&self) -> bool {
        self.inode_type == 2
    }

    /// Test whether the given access bits (`MAY_READ`/`MAY_WRITE`/`MAY_EXEC`)
    /// are permitted for the supplied (uid, gid). uid 0 (root) bypasses checks.
    fn permits(&self, uid: u16, gid: u16, want: u16) -> bool {
        if uid == 0 {
            return true; // root: full access
        }
        // Pick the most specific permission class.
        let granted = if self.uid == uid {
            (self.mode >> 6) & 0o7 // owner bits
        } else if self.gid == gid {
            (self.mode >> 3) & 0o7 // group bits
        } else {
            self.mode & 0o7 // other bits
        };
        (granted & want) == want
    }

    fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        unsafe {
            core::ptr::copy_nonoverlapping(
                self as *const _ as *const u8,
                buf.as_mut_ptr(),
                Self::SIZE,
            );
        }
        buf
    }

    fn from_bytes(buf: &[u8]) -> Self {
        let mut inode = Inode {
            inode_type: 0,
            _pad: 0,
            mode: 0,
            size: 0,
            uid: 0,
            gid: 0,
            mtime: 0,
            direct: [0; DIRECT_PTRS],
            indirect: 0,
            _reserved: [0; 4],
        };
        unsafe {
            core::ptr::copy_nonoverlapping(
                buf.as_ptr(),
                &mut inode as *mut _ as *mut u8,
                Self::SIZE,
            );
        }
        inode
    }
}

/// Directory entry: fixed 64-byte record.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct DirEntry {
    inode: u32,              // inode number (0 = unused entry)
    name: [u8; MAX_NAME_LEN],// null-terminated file name
}

impl DirEntry {
    const SIZE: usize = 64;

    fn new(inode: u32, name: &str) -> Self {
        let mut name_buf = [0u8; MAX_NAME_LEN];
        let bytes = name.as_bytes();
        let n = core::cmp::min(bytes.len(), MAX_NAME_LEN - 1);
        name_buf[..n].copy_from_slice(&bytes[..n]);
        DirEntry {
            inode,
            name: name_buf,
        }
    }

    fn is_used(&self) -> bool {
        self.inode != 0
    }

    fn name_str(&self) -> &str {
        let len = self.name.iter().position(|&b| b == 0).unwrap_or(MAX_NAME_LEN);
        core::str::from_utf8(&self.name[..len]).unwrap_or("")
    }

    fn name_matches(&self, name: &str) -> bool {
        self.name_str() == name
    }

    fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        unsafe {
            core::ptr::copy_nonoverlapping(
                self as *const _ as *const u8,
                buf.as_mut_ptr(),
                Self::SIZE,
            );
        }
        buf
    }

    fn from_bytes(buf: &[u8]) -> Self {
        let mut entry = DirEntry {
            inode: 0,
            name: [0; MAX_NAME_LEN],
        };
        unsafe {
            core::ptr::copy_nonoverlapping(
                buf.as_ptr(),
                &mut entry as *mut _ as *mut u8,
                Self::SIZE,
            );
        }
        entry
    }
}

// ---------------------------------------------------------------------------
// In-memory file system state
// ---------------------------------------------------------------------------

/// A mounted NextFS instance.
pub struct NextFS {
    dev_idx: usize,            // index in the block device registry
    sb: Superblock,            // cached superblock
    block_bitmap: Vec<u8>,     // in-memory free-block bitmap
    inode_bitmap: Vec<u8>,     // in-memory free-inode bitmap (derived)
}

impl NextFS {
    /// Format a block device with a fresh NextFS.
    ///
    /// Reserves ~1% of blocks for inodes (minimum 64 inodes), writes the
    /// superblock + empty bitmaps + empty inode table, and creates the root
    /// directory (inode 1) with "." and ".." entries.
    pub fn format(dev_idx: usize) -> Result<(), FsError> {
        // Get device info from the public API.
        let devs = block::list();
        let dev_info = devs.get(dev_idx).ok_or(FsError::DeviceNotFound)?;
        let total_blocks = dev_info.block_count as u32;
        let bs = dev_info.block_size;

        // We expect 512-byte blocks to match SATA sector size.
        if bs != BLOCK_SIZE {
            return Err(FsError::UnsupportedBlockSize);
        }
        if total_blocks < 10 {
            return Err(FsError::DeviceTooSmall);
        }

        // Reserve ~1% for inodes, minimum 64.
        let inode_count = core::cmp::max(64, total_blocks / 100);
        let inodes_per_block = (BLOCK_SIZE / Inode::SIZE) as u32;
        let inode_table_blocks = (inode_count + inodes_per_block - 1) / inodes_per_block;

        // Bitmap size: 1 bit per block, rounded up to full blocks.
        let bitmap_bits = total_blocks as usize;
        let bitmap_bytes = (bitmap_bits + 7) / 8;
        let bitmap_blocks = ((bitmap_bytes + BLOCK_SIZE - 1) / BLOCK_SIZE) as u32;

        let bitmap_start = 1u32;
        let inode_table_start = bitmap_start + bitmap_blocks;
        let data_start = inode_table_start + inode_table_blocks;

        if data_start >= total_blocks {
            return Err(FsError::DeviceTooSmall);
        }

        // The root directory consumes the first data block (`data_start`), so it
        // is *not* free. Everything from `data_start + 1` onward is available.
        let free_blocks = total_blocks - data_start - 1;
        let free_inodes = inode_count;

        let sb = Superblock {
            magic: MAGIC,
            block_size: BLOCK_SIZE as u32,
            total_blocks,
            inode_count,
            free_blocks,
            free_inodes: free_inodes - 1, // root takes inode 1
            bitmap_start,
            inode_table_start,
            data_start,
            _reserved: [0; 472],
        };

        serial_println!(
            "[nextfs] format: {} blocks, {} inodes, data @ block {}",
            total_blocks as usize,
            inode_count as usize,
            data_start as usize
        );

        // Write superblock.
        block::write(dev_idx, 0, &sb.to_bytes())?;

        // Initialise the block bitmap. Convention: bit = 1 means FREE, bit = 0
        // means USED (see `alloc_block`). Start with every block free (0xFF)...
        let mut bitmap_buf = vec![0xFFu8; bitmap_blocks as usize * BLOCK_SIZE];
        // ...then mark the root directory's data block (`data_start`) as used so
        // the allocator never hands it out again on a later mount. (Metadata
        // blocks below `data_start` are never scanned by `alloc_block`, so they
        // do not need an explicit bit, but marking the root block is essential.)
        {
            let bit = data_start as usize;
            bitmap_buf[bit / 8] &= !(1u8 << (bit % 8));
        }
        for i in 0..bitmap_blocks {
            let off = i as usize * BLOCK_SIZE;
            block::write(dev_idx, bitmap_start as u64 + i as u64, &bitmap_buf[off..off + BLOCK_SIZE])?;
        }

        // Write empty inode table.
        let empty_inode = Inode {
            inode_type: 0,
            _pad: 0,
            mode: 0,
            size: 0,
            uid: 0,
            gid: 0,
            mtime: 0,
            direct: [0; DIRECT_PTRS],
            indirect: 0,
            _reserved: [0; 4],
        };
        let mut inode_block = vec![0u8; BLOCK_SIZE];
        for i in 0..inodes_per_block {
            let off = i as usize * Inode::SIZE;
            inode_block[off..off + Inode::SIZE].copy_from_slice(&empty_inode.to_bytes());
        }
        for i in 0..inode_table_blocks {
            block::write(dev_idx, inode_table_start as u64 + i as u64, &inode_block)?;
        }

        // Create root directory (inode 1) with "." and ".." entries.
        let mut root_inode = Inode::new_dir();
        root_inode.size = (2 * DirEntry::SIZE) as u64;
        // Allocate one data block for root.
        let root_data_block = data_start; // first data block
        root_inode.direct[0] = root_data_block;

        // Mark root data block as used in bitmap (bit 0 = block data_start).
        // We'll handle this in mount() by marking metadata + root block used.

        // Write root inode (inode 1 = offset 1 in inode table).
        Self::write_inode_raw(dev_idx, inode_table_start, inodes_per_block, 1, &root_inode)?;

        // Write root directory entries.
        let dot = DirEntry::new(ROOT_INODE, ".");
        let dotdot = DirEntry::new(ROOT_INODE, ".."); // root's parent is itself
        let mut root_block = vec![0u8; BLOCK_SIZE];
        root_block[0..DirEntry::SIZE].copy_from_slice(&dot.to_bytes());
        root_block[DirEntry::SIZE..2 * DirEntry::SIZE].copy_from_slice(&dotdot.to_bytes());
        block::write(dev_idx, root_data_block as u64, &root_block)?;

        serial_println!("[nextfs] format complete: root directory created");
        Ok(())
    }

    /// Mount an existing NextFS from a block device.
    pub fn mount(dev_idx: usize) -> Result<Self, FsError> {
        // Read and validate superblock.
        let sb_buf_vec = block::read(dev_idx, 0, 1)?;
        let mut sb_buf = [0u8; BLOCK_SIZE];
        sb_buf.copy_from_slice(&sb_buf_vec);
        let sb = Superblock::from_bytes(&sb_buf);

        if !sb.is_valid() {
            return Err(FsError::InvalidMagic);
        }

        // Copy values before printing (packed struct alignment).
        let (total_blks, inode_cnt, data_st) = (sb.total_blocks, sb.inode_count, sb.data_start);
        serial_println!(
            "[nextfs] mount: {} blocks, {} inodes, data @ {}",
            total_blks,
            inode_cnt,
            data_st
        );

        // Load block bitmap into memory.
        let bitmap_blocks = ((sb.total_blocks as usize + 7) / 8 + BLOCK_SIZE - 1) / BLOCK_SIZE;
        let mut block_bitmap = Vec::new();
        for i in 0..bitmap_blocks {
            let buf = block::read(dev_idx, sb.bitmap_start as u64 + i as u64, 1)?;
            block_bitmap.extend_from_slice(&buf);
        }
        // Trim to exact bit count.
        block_bitmap.truncate((sb.total_blocks as usize + 7) / 8);

        // Build inode bitmap by scanning the inode table.
        let inodes_per_block = (BLOCK_SIZE / Inode::SIZE) as u32;
        let inode_table_blocks = (sb.inode_count + inodes_per_block - 1) / inodes_per_block;
        let inode_bitmap_bytes = (sb.inode_count as usize + 7) / 8;
        let mut inode_bitmap = vec![0u8; inode_bitmap_bytes];

        for block_i in 0..inode_table_blocks {
            let buf = block::read(
                dev_idx,
                sb.inode_table_start as u64 + block_i as u64,
                1,
            )?;
            for slot in 0..inodes_per_block {
                let inode_num = block_i * inodes_per_block + slot;
                if inode_num >= sb.inode_count {
                    break;
                }
                let off = slot as usize * Inode::SIZE;
                let inode = Inode::from_bytes(&buf[off..off + Inode::SIZE]);
                if !inode.is_free() {
                    let byte = inode_num as usize / 8;
                    let bit = inode_num as usize % 8;
                    inode_bitmap[byte] |= 1 << bit;
                }
            }
        }

        Ok(NextFS {
            dev_idx,
            sb,
            block_bitmap,
            inode_bitmap,
        })
    }

    /// Write an inode to disk (helper used during format).
    fn write_inode_raw(
        dev_idx: usize,
        inode_table_start: u32,
        inodes_per_block: u32,
        inode_num: u32,
        inode: &Inode,
    ) -> Result<(), FsError> {
        let block_i = inode_num / inodes_per_block;
        let slot = inode_num % inodes_per_block;
        let block_num = inode_table_start + block_i;

        // Read-modify-write the inode table block.
        let mut buf = block::read(dev_idx, block_num as u64, 1)?;
        let off = slot as usize * Inode::SIZE;
        buf[off..off + Inode::SIZE].copy_from_slice(&inode.to_bytes());
        block::write(dev_idx, block_num as u64, &buf)?;
        Ok(())
    }

    /// Allocate a free data block. Returns block number or error.
    fn alloc_block(&mut self) -> Result<u32, FsError> {
        if self.sb.free_blocks == 0 {
            return Err(FsError::NoSpace);
        }
        // First-fit: scan bitmap starting from data_start.
        let start_bit = self.sb.data_start as usize;
        let end_bit = self.sb.total_blocks as usize;
        for bit in start_bit..end_bit {
            let byte = bit / 8;
            let shift = bit % 8;
            if (self.block_bitmap[byte] & (1 << shift)) != 0 {
                // Block is free; mark it used.
                self.block_bitmap[byte] &= !(1 << shift);
                self.sb.free_blocks -= 1;
                return Ok(bit as u32);
            }
        }
        Err(FsError::NoSpace)
    }

    /// Free a data block.
    fn free_block(&mut self, block_num: u32) {
        let bit = block_num as usize;
        let byte = bit / 8;
        let shift = bit % 8;
        self.block_bitmap[byte] |= 1 << shift;
        self.sb.free_blocks += 1;
    }

    /// Allocate a free inode. Returns inode number or error.
    fn alloc_inode(&mut self) -> Result<u32, FsError> {
        if self.sb.free_inodes == 0 {
            return Err(FsError::NoSpace);
        }
        // Scan inode bitmap (skip inode 0).
        for i in 1..self.sb.inode_count {
            let byte = i as usize / 8;
            let bit = i as usize % 8;
            if (self.inode_bitmap[byte] & (1 << bit)) == 0 {
                // Inode is free; mark it used.
                self.inode_bitmap[byte] |= 1 << bit;
                self.sb.free_inodes -= 1;
                return Ok(i);
            }
        }
        Err(FsError::NoSpace)
    }

    /// Free an inode.
    fn free_inode(&mut self, inode_num: u32) {
        let byte = inode_num as usize / 8;
        let bit = inode_num as usize % 8;
        self.inode_bitmap[byte] &= !(1 << bit);
        self.sb.free_inodes += 1;
    }

    /// Read an inode from disk.
    pub fn read_inode(&self, inode_num: u32) -> Result<Inode, FsError> {
        if inode_num == 0 || inode_num >= self.sb.inode_count {
            return Err(FsError::InvalidInode);
        }
        let inodes_per_block = (BLOCK_SIZE / Inode::SIZE) as u32;
        let block_i = inode_num / inodes_per_block;
        let slot = inode_num % inodes_per_block;
        let block_num = self.sb.inode_table_start + block_i;

        let buf = block::read(self.dev_idx, block_num as u64, 1)?;
        let off = slot as usize * Inode::SIZE;
        Ok(Inode::from_bytes(&buf[off..off + Inode::SIZE]))
    }

    /// Write an inode to disk.
    pub fn write_inode(&self, inode_num: u32, inode: &Inode) -> Result<(), FsError> {
        if inode_num == 0 || inode_num >= self.sb.inode_count {
            return Err(FsError::InvalidInode);
        }
        let inodes_per_block = (BLOCK_SIZE / Inode::SIZE) as u32;
        Self::write_inode_raw(
            self.dev_idx,
            self.sb.inode_table_start,
            inodes_per_block,
            inode_num,
            inode,
        )
    }
}

// ---------------------------------------------------------------------------
// File system errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    DeviceNotFound,
    DeviceTooSmall,
    UnsupportedBlockSize,
    InvalidMagic,
    InvalidInode,
    NoSpace,
    NotFound,
    NotDirectory,
    NotFile,
    AlreadyExists,
    NameTooLong,
    NotMounted,
    InvalidOperation,
    DirectoryNotEmpty,
    PermissionDenied,
    BlockError(BlockError),
}

impl From<BlockError> for FsError {
    fn from(e: BlockError) -> Self {
        FsError::BlockError(e)
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            FsError::DeviceNotFound => write!(f, "device not found"),
            FsError::DeviceTooSmall => write!(f, "device too small"),
            FsError::UnsupportedBlockSize => write!(f, "unsupported block size"),
            FsError::InvalidMagic => write!(f, "invalid magic (not a NextFS)"),
            FsError::InvalidInode => write!(f, "invalid inode number"),
            FsError::NoSpace => write!(f, "no space left"),
            FsError::NotFound => write!(f, "not found"),
            FsError::NotDirectory => write!(f, "not a directory"),
            FsError::NotFile => write!(f, "not a file"),
            FsError::AlreadyExists => write!(f, "already exists"),
            FsError::NameTooLong => write!(f, "name too long"),
            FsError::NotMounted => write!(f, "no filesystem mounted"),
            FsError::InvalidOperation => write!(f, "invalid operation"),
            FsError::DirectoryNotEmpty => write!(f, "directory not empty"),
            FsError::PermissionDenied => write!(f, "permission denied"),
            FsError::BlockError(e) => write!(f, "block error: {:?}", e),
        }
    }
}

/// Metadata snapshot of an inode, returned by [`NextFS::stat`].
#[derive(Debug, Clone, Copy)]
pub struct FileStat {
    pub inode: u32,
    pub inode_type: u8, // 1 = file, 2 = directory
    pub mode: u16,      // permission bits (lower 9)
    pub uid: u16,
    pub gid: u16,
    pub size: u64,
    pub mtime: u32, // timer ticks at last modification
}

impl FileStat {
    /// True if this is a directory.
    pub fn is_dir(&self) -> bool {
        self.inode_type == 2
    }

    /// Render the permission bits as a `rwxr-xr-x`-style string.
    pub fn mode_string(&self) -> [u8; 10] {
        let mut s = *b"----------";
        s[0] = if self.is_dir() { b'd' } else { b'-' };
        let bits = [
            (S_IRUSR, b'r'),
            (S_IWUSR, b'w'),
            (S_IXUSR, b'x'),
            (S_IRGRP, b'r'),
            (S_IWGRP, b'w'),
            (S_IXGRP, b'x'),
            (S_IROTH, b'r'),
            (S_IWOTH, b'w'),
            (S_IXOTH, b'x'),
        ];
        for (i, (mask, ch)) in bits.iter().enumerate() {
            if self.mode & mask != 0 {
                s[i + 1] = *ch;
            }
        }
        s
    }
}

/// File open mode.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    Read,
    Write,
    ReadWrite,
}


// ---------------------------------------------------------------------------
// Directory operations
// ---------------------------------------------------------------------------

impl NextFS {
    /// Look up a file name in a directory. Returns inode number if found.
    pub fn dir_lookup(&self, dir_inode: u32, name: &str) -> Result<u32, FsError> {
        let inode = self.read_inode(dir_inode)?;
        if !inode.is_dir() {
            return Err(FsError::NotDirectory);
        }

        let entries_per_block = BLOCK_SIZE / DirEntry::SIZE;
        let total_entries = inode.size as usize / DirEntry::SIZE;

        for entry_i in 0..total_entries {
            let block_i = entry_i / entries_per_block;
            let slot = entry_i % entries_per_block;

            // Read the data block containing this entry.
            let block_num = self.get_inode_block(&inode, block_i as u32)?;
            if block_num == 0 {
                break; // sparse/unallocated
            }
            let buf = block::read(self.dev_idx, block_num as u64, 1)?;
            let off = slot * DirEntry::SIZE;
            let entry = DirEntry::from_bytes(&buf[off..off + DirEntry::SIZE]);

            if entry.is_used() && entry.name_str() == name {
                return Ok(entry.inode);
            }
        }
        Err(FsError::NotFound)
    }

    /// List all entries in a directory. Returns (name, inode) pairs.
    pub fn dir_list(&self, dir_inode: u32) -> Result<Vec<(String, u32)>, FsError> {
        let inode = self.read_inode(dir_inode)?;
        if !inode.is_dir() {
            return Err(FsError::NotDirectory);
        }

        let mut result = Vec::new();
        let entries_per_block = BLOCK_SIZE / DirEntry::SIZE;
        let total_entries = inode.size as usize / DirEntry::SIZE;

        for entry_i in 0..total_entries {
            let block_i = entry_i / entries_per_block;
            let slot = entry_i % entries_per_block;
            let block_num = self.get_inode_block(&inode, block_i as u32)?;
            if block_num == 0 {
                break;
            }
            let buf = block::read(self.dev_idx, block_num as u64, 1)?;
            let off = slot * DirEntry::SIZE;
            let entry = DirEntry::from_bytes(&buf[off..off + DirEntry::SIZE]);
            if entry.is_used() {
                result.push((entry.name_str().to_string(), entry.inode));
            }
        }
        Ok(result)
    }

    /// Add a new entry to a directory.
    pub fn dir_add_entry(&mut self, dir_inode: u32, name: &str, target_inode: u32) -> Result<(), FsError> {
        if name.len() >= MAX_NAME_LEN {
            return Err(FsError::NameTooLong);
        }
        // Check if name already exists.
        if self.dir_lookup(dir_inode, name).is_ok() {
            return Err(FsError::AlreadyExists);
        }

        let mut inode = self.read_inode(dir_inode)?;
        if !inode.is_dir() {
            return Err(FsError::NotDirectory);
        }

        let entries_per_block = BLOCK_SIZE / DirEntry::SIZE;
        let total_entries = inode.size as usize / DirEntry::SIZE;

        // Try to find an unused slot.
        for entry_i in 0..total_entries {
            let block_i = entry_i / entries_per_block;
            let slot = entry_i % entries_per_block;
            let block_num = self.get_inode_block(&inode, block_i as u32)?;
            if block_num == 0 {
                break;
            }
            let mut buf = block::read(self.dev_idx, block_num as u64, 1)?;
            let off = slot * DirEntry::SIZE;
            let entry = DirEntry::from_bytes(&buf[off..off + DirEntry::SIZE]);
            if !entry.is_used() {
                // Reuse this slot.
                let new_entry = DirEntry::new(target_inode, name);
                buf[off..off + DirEntry::SIZE].copy_from_slice(&new_entry.to_bytes());
                block::write(self.dev_idx, block_num as u64, &buf)?;
                return Ok(());
            }
        }

        // No free slot; append a new entry (may need to allocate a new block).
        let new_entry = DirEntry::new(target_inode, name);
        let offset = inode.size;
        self.write_inode_data(&mut inode, offset, &new_entry.to_bytes())?;
        inode.size += DirEntry::SIZE as u64;
        self.write_inode(dir_inode, &inode)?;
        Ok(())
    }

    /// Create a new file in a directory.
    pub fn create_file(&mut self, dir_inode: u32, name: &str) -> Result<u32, FsError> {
        let new_inode_num = self.alloc_inode()?;
        let mut new_inode = Inode::new_file();
        new_inode.mtime = now_ticks();
        self.write_inode(new_inode_num, &new_inode)?;
        match self.dir_add_entry(dir_inode, name, new_inode_num) {
            Ok(()) => Ok(new_inode_num),
            Err(e) => {
                // Rollback: free the inode.
                self.free_inode(new_inode_num);
                Err(e)
            }
        }
    }

    /// Create a new directory in a directory.
    pub fn create_dir(&mut self, parent_inode: u32, name: &str) -> Result<u32, FsError> {
        let new_inode_num = self.alloc_inode()?;
        let mut new_inode = Inode::new_dir();
        new_inode.mtime = now_ticks();

        // Allocate one data block for "." and ".." entries.
        let data_block = self.alloc_block()?;
        new_inode.direct[0] = data_block;
        new_inode.size = (2 * DirEntry::SIZE) as u64;

        let dot = DirEntry::new(new_inode_num, ".");
        let dotdot = DirEntry::new(parent_inode, "..");
        let mut block_buf = vec![0u8; BLOCK_SIZE];
        block_buf[0..DirEntry::SIZE].copy_from_slice(&dot.to_bytes());
        block_buf[DirEntry::SIZE..2 * DirEntry::SIZE].copy_from_slice(&dotdot.to_bytes());
        block::write(self.dev_idx, data_block as u64, &block_buf)?;

        self.write_inode(new_inode_num, &new_inode)?;
        match self.dir_add_entry(parent_inode, name, new_inode_num) {
            Ok(()) => Ok(new_inode_num),
            Err(e) => {
                // Rollback: free the inode and block.
                self.free_block(data_block);
                self.free_inode(new_inode_num);
                Err(e)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// File I/O operations
// ---------------------------------------------------------------------------

impl NextFS {
    /// Get the block number for a given logical block index in an inode's data.
    /// Returns 0 if the block is not allocated (sparse file).
    fn get_inode_block(&self, inode: &Inode, block_idx: u32) -> Result<u32, FsError> {
        if block_idx < DIRECT_PTRS as u32 {
            return Ok(inode.direct[block_idx as usize]);
        }
        // Indirect block.
        let indirect_idx = block_idx - DIRECT_PTRS as u32;
        if inode.indirect == 0 {
            return Ok(0); // not allocated
        }
        // Read the indirect block (array of u32 block numbers).
        let buf = block::read(self.dev_idx, inode.indirect as u64, 1)?;
        let ptrs_per_block = BLOCK_SIZE / 4;
        if indirect_idx >= ptrs_per_block as u32 {
            return Err(FsError::NoSpace); // beyond single-indirect limit
        }
        let off = indirect_idx as usize * 4;
        let block_num = u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
        Ok(block_num)
    }

    /// Allocate and assign a block to an inode's logical block index.
    fn alloc_inode_block(&mut self, inode: &mut Inode, block_idx: u32) -> Result<u32, FsError> {
        if block_idx < DIRECT_PTRS as u32 {
            let block_num = self.alloc_block()?;
            inode.direct[block_idx as usize] = block_num;
            return Ok(block_num);
        }
        // Indirect block.
        let indirect_idx = block_idx - DIRECT_PTRS as u32;
        let ptrs_per_block = BLOCK_SIZE / 4;
        if indirect_idx >= ptrs_per_block as u32 {
            return Err(FsError::NoSpace);
        }

        // Allocate indirect block if needed.
        if inode.indirect == 0 {
            inode.indirect = self.alloc_block()?;
            // Zero it out.
            let zero_block = vec![0u8; BLOCK_SIZE];
            block::write(self.dev_idx, inode.indirect as u64, &zero_block)?;
        }

        // Allocate the data block and update the indirect block.
        let data_block = self.alloc_block()?;
        let mut buf = block::read(self.dev_idx, inode.indirect as u64, 1)?;
        let off = indirect_idx as usize * 4;
        buf[off..off + 4].copy_from_slice(&data_block.to_le_bytes());
        block::write(self.dev_idx, inode.indirect as u64, &buf)?;
        Ok(data_block)
    }

    /// Read data from an inode starting at `offset`.
    pub fn read_inode_data(&self, inode_num: u32, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        let inode = self.read_inode(inode_num)?;
        if !inode.is_file() {
            return Err(FsError::NotFile);
        }
        if offset >= inode.size {
            return Ok(0); // EOF
        }
        let to_read = core::cmp::min(buf.len(), (inode.size - offset) as usize);
        let mut read_so_far = 0usize;

        while read_so_far < to_read {
            let file_pos = offset + read_so_far as u64;
            let block_idx = (file_pos / BLOCK_SIZE as u64) as u32;
            let block_off = (file_pos % BLOCK_SIZE as u64) as usize;
            let block_num = self.get_inode_block(&inode, block_idx)?;
            if block_num == 0 {
                // Sparse block: return zeros.
                let chunk = core::cmp::min(to_read - read_so_far, BLOCK_SIZE - block_off);
                buf[read_so_far..read_so_far + chunk].fill(0);
                read_so_far += chunk;
            } else {
                let block_buf = block::read(self.dev_idx, block_num as u64, 1)?;
                let chunk = core::cmp::min(to_read - read_so_far, BLOCK_SIZE - block_off);
                buf[read_so_far..read_so_far + chunk]
                    .copy_from_slice(&block_buf[block_off..block_off + chunk]);
                read_so_far += chunk;
            }
        }
        Ok(to_read)
    }

    /// Write data to an inode starting at `offset`. Grows the file if needed.
    pub fn write_inode_data(&mut self, inode: &mut Inode, offset: u64, data: &[u8]) -> Result<usize, FsError> {
        if !inode.is_file() && !inode.is_dir() {
            return Err(FsError::NotFile);
        }
        let mut written = 0usize;

        while written < data.len() {
            let file_pos = offset + written as u64;
            let block_idx = (file_pos / BLOCK_SIZE as u64) as u32;
            let block_off = (file_pos % BLOCK_SIZE as u64) as usize;
            let mut block_num = self.get_inode_block(inode, block_idx)?;
            if block_num == 0 {
                // Allocate a new block.
                block_num = self.alloc_inode_block(inode, block_idx)?;
                // Zero it out before partial writes.
                let zero_block = vec![0u8; BLOCK_SIZE];
                block::write(self.dev_idx, block_num as u64, &zero_block)?;
            }
            // Read-modify-write the block.
            let mut block_buf = block::read(self.dev_idx, block_num as u64, 1)?;
            let chunk = core::cmp::min(data.len() - written, BLOCK_SIZE - block_off);
            block_buf[block_off..block_off + chunk].copy_from_slice(&data[written..written + chunk]);
            block::write(self.dev_idx, block_num as u64, &block_buf)?;
            written += chunk;
        }

        // Update file size if we wrote past the end.
        if offset + written as u64 > inode.size {
            inode.size = offset + written as u64;
        }
        // Stamp modification time on any successful write.
        if written > 0 {
            inode.mtime = now_ticks();
        }
        Ok(written)
    }

    /// Read entire file contents into a Vec.
    pub fn read_file(&self, inode_num: u32) -> Result<Vec<u8>, FsError> {
        let inode = self.read_inode(inode_num)?;
        if !inode.is_file() {
            return Err(FsError::NotFile);
        }
        let mut buf = vec![0u8; inode.size as usize];
        self.read_inode_data(inode_num, 0, &mut buf)?;
        Ok(buf)
    }

    /// Write/replace entire file contents.
    pub fn write_file(&mut self, inode_num: u32, data: &[u8]) -> Result<(), FsError> {
        let mut inode = self.read_inode(inode_num)?;
        if !inode.is_file() {
            return Err(FsError::NotFile);
        }
        // Truncate to 0 (freeing blocks), then write.
        self.truncate_inode(&mut inode, 0)?;
        self.write_inode_data(&mut inode, 0, data)?;
        self.write_inode(inode_num, &inode)?;
        Ok(())
    }

    /// Free all data blocks associated with an inode (including indirect blocks).
    fn free_inode_blocks(&mut self, inode: &mut Inode) -> Result<(), FsError> {
        // Free direct blocks.
        for i in 0..DIRECT_PTRS {
            if inode.direct[i] != 0 {
                self.free_block(inode.direct[i]);
                inode.direct[i] = 0;
            }
        }

        // Free indirect blocks.
        if inode.indirect != 0 {
            // Read the indirect block to get all data block pointers.
            let buf = block::read(self.dev_idx, inode.indirect as u64, 1)?;
            let ptrs_per_block = BLOCK_SIZE / 4;
            for i in 0..ptrs_per_block {
                let off = i * 4;
                let block_num = u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
                if block_num != 0 {
                    self.free_block(block_num);
                }
            }
            // Free the indirect block itself.
            self.free_block(inode.indirect);
            inode.indirect = 0;
        }

        Ok(())
    }

    /// Truncate a file to a specific size, freeing blocks beyond the new size.
    pub fn truncate(&mut self, inode_num: u32, new_size: u64) -> Result<(), FsError> {
        let mut inode = self.read_inode(inode_num)?;
        if !inode.is_file() {
            return Err(FsError::NotFile);
        }
        self.truncate_inode(&mut inode, new_size)?;
        self.write_inode(inode_num, &inode)?;
        Ok(())
    }

    /// Internal truncate that operates on an in-memory inode.
    fn truncate_inode(&mut self, inode: &mut Inode, new_size: u64) -> Result<(), FsError> {
        if new_size >= inode.size {
            // Growing or staying the same - just update size.
            inode.size = new_size;
            return Ok(());
        }

        // Shrinking - need to free blocks beyond new_size.
        let old_last_block = if inode.size == 0 {
            0
        } else {
            ((inode.size - 1) / BLOCK_SIZE as u64) as u32
        };
        let new_last_block = if new_size == 0 {
            0
        } else {
            ((new_size - 1) / BLOCK_SIZE as u64) as u32
        };

        // Free direct blocks beyond new_last_block.
        for i in (new_last_block + 1)..(DIRECT_PTRS as u32).min(old_last_block + 1) {
            if inode.direct[i as usize] != 0 {
                self.free_block(inode.direct[i as usize]);
                inode.direct[i as usize] = 0;
            }
        }

        // Handle indirect blocks if we're shrinking below DIRECT_PTRS blocks.
        if new_last_block < DIRECT_PTRS as u32 && inode.indirect != 0 {
            if new_size == 0 || new_last_block < DIRECT_PTRS as u32 {
                // Free all indirect blocks.
                let buf = block::read(self.dev_idx, inode.indirect as u64, 1)?;
                let ptrs_per_block = BLOCK_SIZE / 4;
                let start_indirect = if new_last_block >= DIRECT_PTRS as u32 {
                    new_last_block - DIRECT_PTRS as u32 + 1
                } else {
                    0
                };
                for i in start_indirect..ptrs_per_block.min((old_last_block - DIRECT_PTRS as u32 + 1) as usize) as u32 {
                    let off = i as usize * 4;
                    let block_num = u32::from_le_bytes([
                        buf[off],
                        buf[off + 1],
                        buf[off + 2],
                        buf[off + 3],
                    ]);
                    if block_num != 0 {
                        self.free_block(block_num);
                    }
                }
                // If no indirect blocks remain, free the indirect block itself.
                if new_last_block < DIRECT_PTRS as u32 {
                    self.free_block(inode.indirect);
                    inode.indirect = 0;
                }
            }
        } else if old_last_block >= DIRECT_PTRS as u32 && inode.indirect != 0 {
            // Shrinking within indirect range - free some indirect blocks.
            let buf = block::read(self.dev_idx, inode.indirect as u64, 1)?;
            let start_indirect = if new_last_block >= DIRECT_PTRS as u32 {
                new_last_block - DIRECT_PTRS as u32 + 1
            } else {
                0
            };
            let end_indirect = old_last_block - DIRECT_PTRS as u32 + 1;
            for i in start_indirect..end_indirect {
                let off = i as usize * 4;
                let block_num = u32::from_le_bytes([
                    buf[off],
                    buf[off + 1],
                    buf[off + 2],
                    buf[off + 3],
                ]);
                if block_num != 0 {
                    self.free_block(block_num);
                }
            }
        }

        inode.size = new_size;
        Ok(())
    }

    /// Remove an entry from a directory by name.
    pub fn dir_remove_entry(&mut self, dir_inode: u32, name: &str) -> Result<u32, FsError> {
        let inode = self.read_inode(dir_inode)?;
        if !inode.is_dir() {
            return Err(FsError::NotDirectory);
        }

        let entries_per_block = BLOCK_SIZE / DirEntry::SIZE;
        let total_entries = inode.size as usize / DirEntry::SIZE;

        // Find and remove the entry.
        for entry_i in 0..total_entries {
            let block_i = entry_i / entries_per_block;
            let slot = entry_i % entries_per_block;
            let block_num = self.get_inode_block(&inode, block_i as u32)?;
            if block_num == 0 {
                break;
            }
            let mut buf = block::read(self.dev_idx, block_num as u64, 1)?;
            let off = slot * DirEntry::SIZE;
            let entry = DirEntry::from_bytes(&buf[off..off + DirEntry::SIZE]);
            if entry.is_used() && entry.name_matches(name) {
                // Found it - mark as unused (inode = 0).
                let target_inode = entry.inode;
                let empty_entry = DirEntry::new(0, "");
                buf[off..off + DirEntry::SIZE].copy_from_slice(&empty_entry.to_bytes());
                block::write(self.dev_idx, block_num as u64, &buf)?;
                return Ok(target_inode);
            }
        }

        Err(FsError::NotFound)
    }

    /// Unlink (delete) a file from a directory.
    pub fn unlink(&mut self, dir_inode: u32, name: &str) -> Result<(), FsError> {
        // Special protection for "." and "..".
        if name == "." || name == ".." {
            return Err(FsError::InvalidOperation);
        }

        // Remove the directory entry and get the target inode.
        let target_inode = self.dir_remove_entry(dir_inode, name)?;

        // Read the target inode and verify it's a file.
        let mut inode = self.read_inode(target_inode)?;
        if !inode.is_file() {
            // Put the entry back if it wasn't a file.
            let _ = self.dir_add_entry(dir_inode, name, target_inode);
            return Err(FsError::NotFile);
        }

        // Free all data blocks.
        self.free_inode_blocks(&mut inode)?;

        // Mark inode as free.
        inode.inode_type = 0;
        inode.size = 0;
        self.write_inode(target_inode, &inode)?;
        self.free_inode(target_inode);

        Ok(())
    }

    /// Remove an empty directory.
    pub fn rmdir(&mut self, parent_inode: u32, name: &str) -> Result<(), FsError> {
        // Special protection for "." and "..".
        if name == "." || name == ".." {
            return Err(FsError::InvalidOperation);
        }

        // First, look up the target to check if it's empty.
        let target_inode = self.dir_lookup(parent_inode, name)?;
        let mut inode = self.read_inode(target_inode)?;

        if !inode.is_dir() {
            return Err(FsError::NotDirectory);
        }

        // Check if directory is empty (should only have "." and "..").
        let entries = self.dir_list(target_inode)?;
        if entries.len() > 2 {
            return Err(FsError::DirectoryNotEmpty);
        }

        // Remove the entry from the parent directory.
        self.dir_remove_entry(parent_inode, name)?;

        // Free all data blocks (should just be the "." and ".." block).
        self.free_inode_blocks(&mut inode)?;

        // Mark inode as free.
        inode.inode_type = 0;
        inode.size = 0;
        self.write_inode(target_inode, &inode)?;
        self.free_inode(target_inode);

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Permission / ownership / metadata
    // -----------------------------------------------------------------------

    /// Return a metadata snapshot (`stat`) for an inode.
    pub fn stat(&self, inode_num: u32) -> Result<FileStat, FsError> {
        let inode = self.read_inode(inode_num)?;
        if inode.is_free() {
            return Err(FsError::NotFound);
        }
        Ok(FileStat {
            inode: inode_num,
            inode_type: inode.inode_type,
            mode: inode.mode,
            uid: inode.uid,
            gid: inode.gid,
            size: inode.size,
            mtime: inode.mtime,
        })
    }

    /// Change the permission bits of an inode (only the lower 9 bits are kept).
    pub fn chmod(&mut self, inode_num: u32, mode: u16) -> Result<(), FsError> {
        let mut inode = self.read_inode(inode_num)?;
        if inode.is_free() {
            return Err(FsError::NotFound);
        }
        inode.mode = mode & 0o777;
        inode.mtime = now_ticks();
        self.write_inode(inode_num, &inode)
    }

    /// Change the owner (uid) and group (gid) of an inode.
    pub fn chown(&mut self, inode_num: u32, uid: u16, gid: u16) -> Result<(), FsError> {
        let mut inode = self.read_inode(inode_num)?;
        if inode.is_free() {
            return Err(FsError::NotFound);
        }
        inode.uid = uid;
        inode.gid = gid;
        inode.mtime = now_ticks();
        self.write_inode(inode_num, &inode)
    }

    /// Set the owner/group/mode of a freshly created inode in one shot.
    pub fn set_owner_mode(
        &mut self,
        inode_num: u32,
        uid: u16,
        gid: u16,
        mode: u16,
    ) -> Result<(), FsError> {
        let mut inode = self.read_inode(inode_num)?;
        if inode.is_free() {
            return Err(FsError::NotFound);
        }
        inode.uid = uid;
        inode.gid = gid;
        inode.mode = mode & 0o777;
        self.write_inode(inode_num, &inode)
    }

    /// Verify that (uid, gid) may perform `want` (`MAY_READ`/`MAY_WRITE`/
    /// `MAY_EXEC`, OR-combined) on the inode. Returns `PermissionDenied` if not.
    pub fn check_permission(
        &self,
        inode_num: u32,
        uid: u16,
        gid: u16,
        want: u16,
    ) -> Result<(), FsError> {
        let inode = self.read_inode(inode_num)?;
        if inode.is_free() {
            return Err(FsError::NotFound);
        }
        if inode.permits(uid, gid, want) {
            Ok(())
        } else {
            Err(FsError::PermissionDenied)
        }
    }

    /// Flush cached superblock and bitmaps to disk.
    pub fn sync(&self) -> Result<(), FsError> {
        // Write superblock.
        block::write(self.dev_idx, 0, &self.sb.to_bytes())?;
        // Write block bitmap.
        let bitmap_blocks = ((self.sb.total_blocks as usize + 7) / 8 + BLOCK_SIZE - 1) / BLOCK_SIZE;
        for i in 0..bitmap_blocks {
            let off = i * BLOCK_SIZE;
            let end = core::cmp::min(off + BLOCK_SIZE, self.block_bitmap.len());
            let mut buf = vec![0u8; BLOCK_SIZE];
            buf[..end - off].copy_from_slice(&self.block_bitmap[off..end]);
            block::write(self.dev_idx, self.sb.bitmap_start as u64 + i as u64, &buf)?;
        }
        Ok(())
    }
}
