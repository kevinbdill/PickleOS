//! ELF binary loader for user-space processes.
//!
//! This module parses 64-bit ELF executables and loads their segments into
//! user-space virtual memory. It supports:
//!   - PT_LOAD segments (code, data, bss)
//!   - Entry point extraction
//!   - Proper permission mapping (read/write/execute)
//!
//! The loader creates a fresh page table for each process, ensuring complete
//! address-space isolation between user processes and the kernel.

use crate::memory;
use crate::serial_println;
use alloc::vec::Vec;
use alloc::string::String;
use alloc::format;
use x86_64::structures::paging::{
    Mapper, Page, PageTableFlags, Size4KiB, Translate,
    mapper::TranslateResult,
};
use x86_64::VirtAddr;

/// Magic bytes that identify a valid ELF file.
const ELF_MAGIC: &[u8; 4] = b"\x7fELF";

/// ELF class: 64-bit.
const ELFCLASS64: u8 = 2;

/// ELF data encoding: little-endian.
const ELFDATA2LSB: u8 = 1;

/// ELF type: executable.
const ET_EXEC: u16 = 2;

/// ELF type: shared object / position-independent executable.
const ET_DYN: u16 = 3;

/// ELF machine: x86-64.
const EM_X86_64: u16 = 62;

/// Program header type: loadable segment.
const PT_LOAD: u32 = 1;

/// Program header type: dynamic linking information.
const PT_DYNAMIC: u32 = 2;

/// Program header flags.
const PF_X: u32 = 1; // Execute
const PF_W: u32 = 2; // Write
const PF_R: u32 = 4; // Read

/// x86-64 relocation types.
const R_X86_64_NONE: u32 = 0;
const R_X86_64_64: u32 = 1;        // Direct 64-bit relocation
const R_X86_64_GLOB_DAT: u32 = 6;  // GOT entry for symbol
const R_X86_64_JUMP_SLOT: u32 = 7; // PLT entry for symbol
const R_X86_64_RELATIVE: u32 = 8;  // Adjust by program base

/// Dynamic section tags.
const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;    // Dependency library name
const DT_PLTGOT: u64 = 3;    // PLT/GOT address
const DT_STRTAB: u64 = 5;    // String table address
const DT_SYMTAB: u64 = 6;    // Symbol table address
const DT_RELA: u64 = 7;      // Relocation table address
const DT_RELASZ: u64 = 8;    // Relocation table size
const DT_RELAENT: u64 = 9;   // Relocation entry size
const DT_STRSZ: u64 = 10;    // String table size
const DT_JMPREL: u64 = 23;   // PLT relocation address

/// Minimal parsed ELF header (enough to load segments).
#[derive(Debug)]
struct ElfHeader {
    entry: u64,
    phoff: u64,
    phnum: u16,
}

/// A loadable segment.
#[derive(Debug, Clone)]
pub struct Segment {
    pub vaddr: u64,
    pub memsz: u64,
    pub filesz: u64,
    pub offset: u64,
    pub flags: u32,
}

/// A relocation entry with addend (RELA format).
#[derive(Debug, Clone)]
struct Relocation {
    offset: u64,   // Where to apply the relocation
    r_type: u32,   // Relocation type
    symbol: u32,   // Symbol index (for symbol-based relocations)
    addend: i64,   // Addend value
}

/// Dynamic section information.
#[derive(Debug, Default)]
struct DynamicInfo {
    rela_addr: Option<u64>,
    rela_size: usize,
    symtab_addr: Option<u64>,
    strtab_addr: Option<u64>,
    strtab_size: usize,
    needed_libs: Vec<String>,
}

/// Result of parsing an ELF binary.
#[derive(Debug)]
pub struct ElfBinary {
    pub entry: u64,
    pub segments: Vec<Segment>,
    relocations: Vec<Relocation>,
    dynamic_info: DynamicInfo,
    is_dynamic: bool,
}

impl ElfBinary {
    /// Parse an ELF binary from raw bytes.
    pub fn parse(data: &[u8]) -> Result<ElfBinary, &'static str> {
        if data.len() < 64 {
            return Err("ELF file too small");
        }

        // Verify ELF magic.
        if &data[0..4] != ELF_MAGIC {
            return Err("Invalid ELF magic");
        }

        // Check class (64-bit) and endianness (little-endian).
        if data[4] != ELFCLASS64 {
            return Err("Not a 64-bit ELF");
        }
        if data[5] != ELFDATA2LSB {
            return Err("Not little-endian");
        }

        // Parse ELF header.
        let e_type = u16::from_le_bytes([data[16], data[17]]);
        let e_machine = u16::from_le_bytes([data[18], data[19]]);
        if e_type != ET_EXEC && e_type != ET_DYN {
            return Err("Not an executable or shared object ELF");
        }
        if e_machine != EM_X86_64 {
            return Err("Not x86-64");
        }

        let entry = u64::from_le_bytes([
            data[24], data[25], data[26], data[27], data[28], data[29], data[30], data[31],
        ]);
        let phoff = u64::from_le_bytes([
            data[32], data[33], data[34], data[35], data[36], data[37], data[38], data[39],
        ]);
        let phnum = u16::from_le_bytes([data[56], data[57]]);

        let header = ElfHeader {
            entry,
            phoff,
            phnum,
        };

        let is_dynamic = e_type == ET_DYN;

        // Parse program headers (segments).
        let mut segments = Vec::new();
        let mut dynamic_segment_offset: Option<u64> = None;
        let mut dynamic_segment_size: usize = 0;
        
        for i in 0..header.phnum {
            let off = (header.phoff + i as u64 * 56) as usize;
            if off + 56 > data.len() {
                return Err("Program header out of bounds");
            }

            let p_type = u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
            
            // Capture PT_DYNAMIC segment information
            if p_type == PT_DYNAMIC {
                let p_offset = u64::from_le_bytes([
                    data[off + 8], data[off + 9], data[off + 10], data[off + 11],
                    data[off + 12], data[off + 13], data[off + 14], data[off + 15],
                ]);
                let p_filesz = u64::from_le_bytes([
                    data[off + 32], data[off + 33], data[off + 34], data[off + 35],
                    data[off + 36], data[off + 37], data[off + 38], data[off + 39],
                ]);
                dynamic_segment_offset = Some(p_offset);
                dynamic_segment_size = p_filesz as usize;
                continue;
            }
            
            if p_type != PT_LOAD {
                continue; // Skip non-loadable segments.
            }

            let p_flags = u32::from_le_bytes([data[off + 4], data[off + 5], data[off + 6], data[off + 7]]);
            let p_offset = u64::from_le_bytes([
                data[off + 8], data[off + 9], data[off + 10], data[off + 11],
                data[off + 12], data[off + 13], data[off + 14], data[off + 15],
            ]);
            let p_vaddr = u64::from_le_bytes([
                data[off + 16], data[off + 17], data[off + 18], data[off + 19],
                data[off + 20], data[off + 21], data[off + 22], data[off + 23],
            ]);
            let p_filesz = u64::from_le_bytes([
                data[off + 32], data[off + 33], data[off + 34], data[off + 35],
                data[off + 36], data[off + 37], data[off + 38], data[off + 39],
            ]);
            let p_memsz = u64::from_le_bytes([
                data[off + 40], data[off + 41], data[off + 42], data[off + 43],
                data[off + 44], data[off + 45], data[off + 46], data[off + 47],
            ]);

            segments.push(Segment {
                vaddr: p_vaddr,
                memsz: p_memsz,
                filesz: p_filesz,
                offset: p_offset,
                flags: p_flags,
            });
        }

        // Parse dynamic section if present
        let mut dynamic_info = DynamicInfo::default();
        let mut relocations = Vec::new();
        
        if let Some(dyn_offset) = dynamic_segment_offset {
            let dyn_start = dyn_offset as usize;
            let dyn_end = dyn_start + dynamic_segment_size;
            
            if dyn_end <= data.len() {
                // Parse dynamic entries (each is 16 bytes: tag + value)
                let mut i = dyn_start;
                while i + 16 <= dyn_end {
                    let tag = u64::from_le_bytes([
                        data[i], data[i+1], data[i+2], data[i+3],
                        data[i+4], data[i+5], data[i+6], data[i+7],
                    ]);
                    let val = u64::from_le_bytes([
                        data[i+8], data[i+9], data[i+10], data[i+11],
                        data[i+12], data[i+13], data[i+14], data[i+15],
                    ]);
                    
                    match tag {
                        DT_NULL => break,
                        DT_RELA => dynamic_info.rela_addr = Some(val),
                        DT_RELASZ => dynamic_info.rela_size = val as usize,
                        DT_SYMTAB => dynamic_info.symtab_addr = Some(val),
                        DT_STRTAB => dynamic_info.strtab_addr = Some(val),
                        DT_STRSZ => dynamic_info.strtab_size = val as usize,
                        DT_NEEDED => {
                            // For now, just record that there's a dependency
                            // Full implementation would parse the string table
                            dynamic_info.needed_libs.push(format!("lib{}", val));
                        },
                        _ => {}, // Ignore other tags for now
                    }
                    i += 16;
                }
                
                // Parse RELA relocations if present
                // Note: This is a simplified implementation
                // Full implementation needs to resolve addresses after loading
                if let Some(rela_addr) = dynamic_info.rela_addr {
                    // We'll process relocations during load, not here
                    serial_println!("elf :: found RELA section at {:#x} (size: {})", 
                                   rela_addr, dynamic_info.rela_size);
                }
            }
        }

        Ok(ElfBinary {
            entry: header.entry,
            segments,
            relocations,
            dynamic_info,
            is_dynamic,
        })
    }

    /// Process relocations for a dynamically loaded binary.
    ///
    /// For ET_DYN binaries, we need to apply relocations to adjust addresses
    /// based on the load base. This handles R_X86_64_RELATIVE and symbol-based
    /// relocations (GLOB_DAT, JUMP_SLOT, 64).
    fn apply_relocations(
        &self,
        data: &[u8],
        base_addr: u64,
        mapper: &mut (impl Mapper<Size4KiB> + Translate),
        mem: &mut crate::memory::MemoryManager,
    ) -> Result<(), &'static str> {
        if !self.is_dynamic {
            return Ok(()); // No relocations needed for static binaries
        }
        
        // Find RELA section in the loaded segments
        if let Some(rela_vaddr) = self.dynamic_info.rela_addr {
            let rela_size = self.dynamic_info.rela_size;
            
            // Each RELA entry is 24 bytes: offset (8), info (8), addend (8)
            let num_relocations = rela_size / 24;
            
            serial_println!("elf :: processing {} relocations at base {:#x}", 
                           num_relocations, base_addr);
            
            // Find the RELA data in the file
            for seg in &self.segments {
                if rela_vaddr >= seg.vaddr && rela_vaddr < seg.vaddr + seg.filesz {
                    let rela_offset = seg.offset + (rela_vaddr - seg.vaddr);
                    let rela_start = rela_offset as usize;
                    
                    for i in 0..num_relocations {
                        let entry_offset = rela_start + i * 24;
                        if entry_offset + 24 > data.len() {
                            break;
                        }
                        
                        let r_offset = u64::from_le_bytes([
                            data[entry_offset], data[entry_offset+1], data[entry_offset+2], 
                            data[entry_offset+3], data[entry_offset+4], data[entry_offset+5],
                            data[entry_offset+6], data[entry_offset+7],
                        ]);
                        let r_info = u64::from_le_bytes([
                            data[entry_offset+8], data[entry_offset+9], data[entry_offset+10],
                            data[entry_offset+11], data[entry_offset+12], data[entry_offset+13],
                            data[entry_offset+14], data[entry_offset+15],
                        ]);
                        let r_addend = i64::from_le_bytes([
                            data[entry_offset+16], data[entry_offset+17], data[entry_offset+18],
                            data[entry_offset+19], data[entry_offset+20], data[entry_offset+21],
                            data[entry_offset+22], data[entry_offset+23],
                        ]);
                        
                        let r_type = (r_info & 0xffffffff) as u32;
                        let r_sym = (r_info >> 32) as u32;
                        
                        // Calculate relocation value based on type
                        let value = match r_type {
                            R_X86_64_RELATIVE => {
                                // value = base + addend
                                base_addr.wrapping_add(r_addend as u64)
                            },
                            R_X86_64_GLOB_DAT | R_X86_64_JUMP_SLOT => {
                                // value = symbol_value
                                let sym_val = self.lookup_symbol(data, r_sym, base_addr);
                                if sym_val == 0 {
                                    serial_println!("elf :: WARNING: undefined symbol index {} for relocation type {}", 
                                                   r_sym, r_type);
                                }
                                sym_val
                            },
                            R_X86_64_64 => {
                                // value = symbol_value + addend
                                let sym_val = self.lookup_symbol(data, r_sym, base_addr);
                                sym_val.wrapping_add(r_addend as u64)
                            },
                            R_X86_64_NONE => {
                                continue; // No relocation needed
                            },
                            _ => {
                                serial_println!("elf :: skipping unsupported relocation type {}", r_type);
                                continue;
                            }
                        };
                        
                        // Write the relocated value to memory
                        let target_vaddr = VirtAddr::new(base_addr + r_offset);
                        
                        // Translate to physical address
                        match mapper.translate(target_vaddr) {
                            TranslateResult::Mapped { frame, offset, .. } => {
                                let phys_addr = frame.start_address() + offset;
                                let kern_virt = mem.phys_to_virt(phys_addr);
                                
                                // Write the 64-bit relocation value
                                unsafe {
                                    core::ptr::write_volatile(kern_virt.as_mut_ptr::<u64>(), value);
                                }
                            },
                            _ => {
                                serial_println!("elf :: ERROR: relocation target {:#x} not mapped", target_vaddr.as_u64());
                                return Err("relocation target not mapped");
                            }
                        }
                    }
                    break;
                }
            }
        }
        
        Ok(())
    }

    /// Look up a symbol value by index.
    ///
    /// For ET_DYN binaries with symbol tables, this reads the symbol table entry
    /// and returns the st_value field. For symbols that need to be resolved from
    /// external libraries, this returns 0 (full dependency resolution not implemented).
    fn lookup_symbol(&self, data: &[u8], symbol_index: u32, base_addr: u64) -> u64 {
        if symbol_index == 0 {
            return 0; // STN_UNDEF (undefined symbol index)
        }
        
        // Find the symbol table
        let symtab_vaddr = match self.dynamic_info.symtab_addr {
            Some(addr) => addr,
            None => return 0, // No symbol table
        };
        
        // Each symbol entry is 24 bytes in ELF64
        let sym_size = 24;
        let sym_offset_in_symtab = (symbol_index as u64) * sym_size;
        
        // Find which segment contains the symbol table
        for seg in &self.segments {
            if symtab_vaddr >= seg.vaddr && symtab_vaddr < seg.vaddr + seg.filesz {
                let symtab_file_offset = seg.offset + (symtab_vaddr - seg.vaddr);
                let sym_entry_offset = (symtab_file_offset + sym_offset_in_symtab) as usize;
                
                if sym_entry_offset + sym_size as usize > data.len() {
                    return 0; // Out of bounds
                }
                
                // ELF64 symbol entry: st_name(4), st_info(1), st_other(1), st_shndx(2), st_value(8), st_size(8)
                // We only need st_value which starts at offset 8
                let st_value = u64::from_le_bytes([
                    data[sym_entry_offset + 8],
                    data[sym_entry_offset + 9],
                    data[sym_entry_offset + 10],
                    data[sym_entry_offset + 11],
                    data[sym_entry_offset + 12],
                    data[sym_entry_offset + 13],
                    data[sym_entry_offset + 14],
                    data[sym_entry_offset + 15],
                ]);
                
                // For ET_DYN, symbol values are relative to load base
                if st_value == 0 {
                    // Symbol is undefined (needs external resolution)
                    return 0;
                } else {
                    return base_addr + st_value;
                }
            }
        }
        
        0 // Symbol table segment not found
    }

    /// Load this ELF into a fresh user-space address space.
    ///
    /// `args` is the argument vector (`argv`) and `envs` the environment
    /// (`envp`) to seed onto the new program's initial stack following the
    /// System V x86-64 process-startup convention (see [`setup_user_stack`]).
    ///
    /// Returns the entry point and the resulting top-of-stack pointer (which
    /// already points at `argc`, ready for `_start`).
    pub fn load(
        &self,
        data: &[u8],
        args: &[&str],
        envs: &[&str],
    ) -> Result<(VirtAddr, VirtAddr), &'static str> {
        serial_println!("elf :: loading binary (entry: {:#x}, dynamic: {})", 
                       self.entry, self.is_dynamic);

        memory::with_memory(|mem| {
            // Create a fresh page table for this process (isolates it from kernel).
            let mut mapper = mem.create_user_mapper();

            // For dynamic binaries (ET_DYN/PIE), we load at a base address
            // For static binaries (ET_EXEC), segments specify absolute addresses
            const DYNAMIC_LOAD_BASE: u64 = 0x40000000; // 1 GB
            let base_addr = if self.is_dynamic {
                DYNAMIC_LOAD_BASE
            } else {
                0
            };

            for seg in &self.segments {
                serial_println!(
                    "elf ::   segment @ {:#x} filesz={:#x} memsz={:#x} flags={:#x}",
                    seg.vaddr, seg.filesz, seg.memsz, seg.flags
                );

                // Calculate page-aligned region.
                // Apply base address offset for dynamic binaries
                let load_vaddr = base_addr + seg.vaddr;
                let start_page: Page<Size4KiB> =
                    Page::containing_address(VirtAddr::new(load_vaddr));
                let end_addr = load_vaddr + seg.memsz;
                let end_page: Page<Size4KiB> =
                    Page::containing_address(VirtAddr::new(end_addr - 1));

                // Map all pages in the segment and copy data.
                for page in Page::range_inclusive(start_page, end_page) {
                    let frame = match mem.allocate_frame() {
                        Some(f) => f,
                        None => {
                            serial_println!("elf :: ERROR: out of physical memory while mapping segment");
                            return Err("out of physical memory");
                        }
                    };

                    // Set page permissions based on segment flags.
                    let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
                    if seg.flags & PF_W != 0 {
                        flags |= PageTableFlags::WRITABLE;
                    }
                    if seg.flags & PF_X == 0 {
                        flags |= PageTableFlags::NO_EXECUTE;
                    }

                    unsafe {
                        if let Err(e) = mapper
                            .map_to(page, frame, flags, mem.frame_allocator())
                            .map_err(|e| {
                                serial_println!("elf :: ERROR: failed to map page {:?}: {:?}", page, e);
                                "failed to map page"
                            }) {
                            return Err(e);
                        } else {
                            // Flush TLB for this page
                            use x86_64::structures::paging::mapper::MapperFlush;
                            MapperFlush::new(page).flush();
                        }
                    }

                    // Zero the frame and copy data for this page.
                    let frame_ptr = mem.phys_to_virt(frame.start_address()).as_mut_ptr::<u8>();
                    unsafe {
                        core::ptr::write_bytes(frame_ptr, 0, 4096);
                    }

                    // Copy file data for this page (if any).
                    //
                    // A LOAD segment's vaddr is not necessarily page-aligned
                    // (e.g. a `.text` at 0x401dd0 sits 0xdd0 bytes into page
                    // 0x401000). We must place the file bytes at the matching
                    // *in-page* offset, not at offset 0 of the frame, otherwise
                    // the entry point lands in zeroed memory. We compute the
                    // intersection of this page with the segment's file-backed
                    // byte range [load_vaddr, load_vaddr + filesz).
                    if seg.filesz > 0 {
                        let page_start = page.start_address().as_u64();
                        let seg_file_start = load_vaddr;
                        let seg_file_end = load_vaddr + seg.filesz;
                        let copy_start = core::cmp::max(page_start, seg_file_start);
                        let copy_end = core::cmp::min(page_start + 4096, seg_file_end);

                        if copy_start < copy_end {
                            let dest_off = (copy_start - page_start) as usize;
                            let file_offset = seg.offset + (copy_start - seg_file_start);
                            let bytes_to_copy = (copy_end - copy_start) as usize;

                            let file_start = file_offset as usize;
                            let file_end = file_start + bytes_to_copy;
                            if file_end > data.len() {
                                return Err("segment data out of bounds");
                            }

                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    data[file_start..file_end].as_ptr(),
                                    frame_ptr.add(dest_off),
                                    bytes_to_copy,
                                );
                            }
                        }
                    }
                }
            }

            // Apply relocations for dynamic binaries
            if self.is_dynamic {
                self.apply_relocations(data, base_addr, &mut mapper, mem)?;
            }

            // Allocate and map a user stack (8 pages = 32 KiB).
            const STACK_PAGES: u64 = 8;
            const USER_STACK_TOP: u64 = 0x7FFF_FFFF_F000;
            let stack_bottom = USER_STACK_TOP - STACK_PAGES * 4096;

            // Kernel-virtual pointer to the highest (top) stack page, plus the
            // user-virtual base of that page. We seed argc/argv/envp here after
            // the loop. The top page covers [USER_STACK_TOP-4096, USER_STACK_TOP).
            let mut top_frame_kptr: *mut u8 = core::ptr::null_mut();
            let top_page_user_base = USER_STACK_TOP - 4096;

            for i in 0..STACK_PAGES {
                let page_addr = stack_bottom + i * 4096;
                let page: Page<Size4KiB> = Page::containing_address(VirtAddr::new(page_addr));
                let frame = match mem.allocate_frame() {
                    Some(f) => f,
                    None => {
                        serial_println!("elf :: ERROR: out of memory for stack page {}", i);
                        return Err("out of memory for stack");
                    }
                };

                let flags = PageTableFlags::PRESENT
                    | PageTableFlags::WRITABLE
                    | PageTableFlags::USER_ACCESSIBLE
                    | PageTableFlags::NO_EXECUTE;

                unsafe {
                    if let Err(e) = mapper
                        .map_to(page, frame, flags, mem.frame_allocator())
                        .map_err(|e| {
                            serial_println!("elf :: ERROR: failed to map stack page {}: {:?}", i, e);
                            "failed to map stack page"
                        }) {
                        return Err(e);
                    } else {
                        use x86_64::structures::paging::mapper::MapperFlush;
                        MapperFlush::new(page).flush();
                    }
                }

                // Zero the stack frame.
                let frame_ptr = mem.phys_to_virt(frame.start_address()).as_mut_ptr::<u8>();
                unsafe {
                    core::ptr::write_bytes(frame_ptr, 0, 4096);
                }

                // Remember the top page's kernel pointer for argv/envp setup.
                if page_addr == top_page_user_base {
                    top_frame_kptr = frame_ptr;
                }
            }

            // Seed argc/argv/envp onto the freshly mapped top stack page. We
            // write through the kernel's identity-style physical mapping
            // (`top_frame_kptr`) rather than the user virtual address, because
            // the new address space is not yet active.
            let user_sp = setup_user_stack(
                top_frame_kptr,
                top_page_user_base,
                4096,
                args,
                envs,
            )?;

            // Store the mapper for this process (will be used when switching to it).
            let mapper_phys = mem.store_user_mapper(mapper);

            // Adjust entry point for dynamic binaries
            let final_entry = base_addr + self.entry;

            serial_println!(
                "elf :: loaded successfully (cr3: {:#x}, sp: {:#x}, entry: {:#x}, argc: {})",
                mapper_phys, user_sp, final_entry, args.len()
            );

            Ok((VirtAddr::new(final_entry), VirtAddr::new(user_sp)))
        })
    }
}

/// Lay out the System V x86-64 process-startup stack into a freshly mapped top
/// stack page and return the resulting user-space stack pointer.
///
/// At program entry the stack must look like (low → high address):
/// ```text
///   [rsp]            argc                      (u64)
///                    argv[0], argv[1], ...      (argc pointers)
///                    NULL                       (argv terminator)
///                    envp[0], envp[1], ...      (envp pointers)
///                    NULL                       (envp terminator)
///                    auxv: AT_NULL, 0           (one empty aux entry)
///                    ... string data ...        (the argv/envp bytes)
/// ```
/// `frame_kptr` is the kernel-virtual pointer to the page backing the user
/// page whose base user address is `page_user_base` and whose length is
/// `page_size`. All argv/envp data must fit within this single page.
///
/// The returned `rsp` is 16-byte aligned per the ABI (so that, after the
/// implicit "return address" convention, `_start` sees a correctly aligned
/// stack).
fn setup_user_stack(
    frame_kptr: *mut u8,
    page_user_base: u64,
    page_size: usize,
    args: &[&str],
    envs: &[&str],
) -> Result<u64, &'static str> {
    if frame_kptr.is_null() {
        return Err("no top stack page captured");
    }

    // 1. Copy the argv then envp strings near the top of the page, recording
    //    each string's user-virtual address.
    let mut off = page_size;
    let mut str_addrs: Vec<u64> = Vec::with_capacity(args.len() + envs.len());
    for s in args.iter().chain(envs.iter()) {
        let bytes = s.as_bytes();
        let need = bytes.len() + 1; // include the NUL terminator
        if need + 256 > off {
            // Leave headroom for the pointer table; bail out if args are huge.
            return Err("argv/envp too large for initial stack page");
        }
        off -= need;
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), frame_kptr.add(off), bytes.len());
            *frame_kptr.add(off + bytes.len()) = 0;
        }
        str_addrs.push(page_user_base + off as u64);
    }

    // 2. Align the string area base down to 16 bytes.
    off &= !0xF;

    let argc = args.len();
    let nenv = envs.len();
    // Word count for the table: argc + argv ptrs + argv NULL + envp ptrs +
    // envp NULL + 2 words for the single AT_NULL auxv entry.
    let words = 1 + argc + 1 + nenv + 1 + 2;
    // Keep the final rsp 16-byte aligned. `off` is 16-aligned; if `words` is
    // odd the table occupies an odd number of 8-byte slots, so insert one pad
    // word above the table to restore alignment.
    if words % 2 != 0 {
        off -= 8;
    }
    let table_bytes = words * 8;
    if table_bytes > off {
        return Err("argv/envp table too large for initial stack page");
    }
    let rsp_off = off - table_bytes;
    let rsp_user = page_user_base + rsp_off as u64;

    // 3. Write the table words.
    let mut w = rsp_off;
    let write_u64 = |o: usize, v: u64| unsafe {
        core::ptr::write(frame_kptr.add(o) as *mut u64, v);
    };
    write_u64(w, argc as u64);
    w += 8;
    for i in 0..argc {
        write_u64(w, str_addrs[i]);
        w += 8;
    }
    write_u64(w, 0); // argv terminator
    w += 8;
    for i in 0..nenv {
        write_u64(w, str_addrs[argc + i]);
        w += 8;
    }
    write_u64(w, 0); // envp terminator
    w += 8;
    write_u64(w, 0); // auxv: AT_NULL type
    w += 8;
    write_u64(w, 0); // auxv: AT_NULL value

    Ok(rsp_user)
}

/// Load an ELF binary from the filesystem and execute it.
///
/// This is a convenience function that:
/// 1. Opens the file via VFS (task 0 context)
/// 2. Reads the entire file into a buffer
/// 3. Parses it as an ELF binary
/// 4. Loads it into user-space memory
/// 5. Returns the entry point and stack pointer
///
/// # Arguments
/// * `path` - Path to the ELF file on NextFS (e.g., "/bin/init")
/// * `args` - argument vector (`argv`) seeded onto the new stack
/// * `envs` - environment vector (`envp`) seeded onto the new stack
///
/// # Returns
/// * `Ok((entry, stack))` - Entry point address and stack top on success
/// * `Err(msg)` - Error message on failure
pub fn load_from_file(
    path: &str,
    args: &[&str],
    envs: &[&str],
) -> Result<(VirtAddr, VirtAddr), &'static str> {
    use alloc::vec::Vec;
    use crate::fs::vfs;
    
    serial_println!("elf :: loading executable from '{}'", path);

    // Reading a program image off disk drives the AHCI controller's MMIO
    // registers. Those accesses are capability-checked against the *current*
    // task, but exec runs in the context of an ordinary user task that holds no
    // device capabilities. This load is performed on the kernel's behalf, so we
    // mark the scope as kernel-mediated I/O for its duration (the guard is
    // dropped automatically on every return path below).
    let _io_guard = crate::driver::mmio::KernelIoGuard::enter();

    // Use task 0 (kernel) context for file operations
    const KERNEL_TASK: u32 = 0;

    // Open the file for reading
    let flags = vfs::OpenFlags {
        read: true,
        write: false,
        create: false,
        truncate: false,
        append: false,
    };

    let fd = vfs::open(KERNEL_TASK, path, flags)
        .map_err(|e| {
            serial_println!("elf :: open('{}') failed: {:?}", path, e);
            "failed to open file"
        })?;

    // Get file size by seeking to end
    let file_size = vfs::seek(KERNEL_TASK, fd, 0, vfs::SeekWhence::End)
        .map_err(|_| "failed to seek to end")? as usize;

    // Seek back to start
    vfs::seek(KERNEL_TASK, fd, 0, vfs::SeekWhence::Set)
        .map_err(|_| "failed to seek to start")?;

    // Allocate buffer and read entire file
    let mut file_data = Vec::with_capacity(file_size);
    file_data.resize(file_size, 0);

    let bytes_read = vfs::read(KERNEL_TASK, fd, &mut file_data)
        .map_err(|_| "failed to read file")?;

    // Close the file
    vfs::close(KERNEL_TASK, fd)
        .map_err(|_| "failed to close file")?;

    if bytes_read != file_size {
        return Err("incomplete read");
    }

    serial_println!("elf :: read {} bytes from disk", file_data.len());

    // Parse and load the ELF binary
    let elf = ElfBinary::parse(&file_data)?;
    elf.load(&file_data, args, envs)
}
