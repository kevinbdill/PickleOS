# PICKLE OS — Phase 3 Status: User-Space Driver Foundation

This document records the first slice of Phase 3 ("Drivers in user space"): the
mechanisms a confined, unprivileged task needs to actually drive hardware —
**IRQ delivery** and **capability-checked I/O** — plus two reference drivers and
the address-space-isolation fix that makes running multiple user tasks robust.

---

## 1. Address-space isolation correctness (carried from Phase 2)

### Symptom
With a second user task spawned, the system would *intermittently* (~1 boot in 4)
take a user-mode page fault at a tiny faulting address (e.g. `0x1f`) shortly
after a syscall that yielded the CPU (notably `SYS_SLEEP`). Most boots were
clean, which pointed at a timing/preemption race rather than a missing mapping.

### Root cause
`context_switch` only swaps the **kernel stack** (callee-saved registers + RSP).
CR3 (the address space) was loaded **only** by `user_task_trampoline`, and only
on a task's *first* run. When an already-running user task was **resumed**, it
kept whatever CR3 happened to be active — frequently *another* task's address
space. Because both user tasks are the same program loaded twice, code addresses
matched but stack/data frames did not, so the resumed task silently executed
against the wrong physical pages → corruption and eventual faults.

### Fix
`scheduler::schedule()` now reloads CR3 on **every** switch:

- User task → its own `user_cr3`.
- Kernel task → the shared `KERNEL_CR3` captured at init.
- The reload is skipped when already in the target CR3 (avoids needless TLB
  flushes). All kernel mappings are shared across every CR3, so the kernel code
  performing the switch continues uninterrupted across the `mov cr3` .

### Verification
8 consecutive headless boots, each running the full workload (two user tasks,
IPC ping/pong, heartbeat, services, VFS self-test, IRQ drivers) for ~80k serial
lines — **zero** page faults. The `SYS_SLEEP` path now reliably prints
`Testing SYS_SLEEP...` → `Slept for ~50 ticks` → `Test complete. Exiting.`

---

## 2. IRQ delivery: ISR → task bridge (`driver/irq.rs`)

A driver task must be woken when its device interrupts, without doing device
work inside the hard ISR. The bridge keeps **lock-free per-line state** (all
`AtomicU64`):

| Field     | Meaning                                                        |
|-----------|----------------------------------------------------------------|
| `owner`   | task id that registered for this IRQ line                      |
| `pending` | count of notifications delivered but not yet consumed          |
| `waiter`  | task id currently blocked in `wait()`, or sentinel "none"      |
| `count`   | lifetime total of notifications (for `irqs` shell command)     |

- `register(irq, task_id)` — a driver claims a line.
- `wait(irq) -> u64` — called from the driver task; sleeps until `pending > 0`,
  returns the number of IRQs observed. Interrupt-safe sleep/wake handshake.
- `notify_from_isr(irq)` — called from the real hardware ISR *after* EOI; bumps
  counters and wakes the waiter. Does no allocation and takes no lock, so it is
  safe in interrupt context.
- `stats(irq)` — owner + counts, surfaced by the shell.

The timer ISR (IRQ0) and keyboard ISR (IRQ1) both call `notify_from_isr` after
sending EOI, *in addition to* the kernel's own use of the timer for preemption —
proving a shared IRQ line can fan out to a user-space-style driver without
disturbing the kernel.

---

## 3. Capability-checked port I/O (`driver/portio.rs`, `capability.rs`)

A new capability object models I/O-port authority:

```rust
Object::Port { base: u16, count: u16 }
```

`portio::inb(port)` / `outb(port, val)` look up the calling task's capabilities
for a `Port` object that (a) covers `port` and (b) carries the required
`READ`/`WRITE` rights, via the new `capability::find_object(task, pred, rights)`
helper. Without the capability the access is refused — a driver cannot touch
ports it was not granted. This is the same no-ambient-authority model used
elsewhere in PICKLE OS, now extended to raw hardware I/O.

---

## 4. Reference drivers

### PS/2 keyboard (`driver/keyboard.rs`)
A ring-3-style driver task that:
1. mints and holds its own capabilities for **IRQ1** and **port 0x60**;
2. blocks in `irq::wait(1)`;
3. on wake, reads the scancode through the **capability-checked** `inb(0x60)`;
4. forwards the byte to the shell input path.

The kernel keyboard ISR no longer reads the data port itself — it only sends EOI
and `notify_from_isr(1)`. The byte stays latched in the controller's output
buffer until the driver drains it, so nothing is lost while the driver is
scheduled. Validated by injecting keystrokes via the QEMU monitor (`sendkey`)
and observing the correct scancodes flow through the full
IRQ1 → wake → capability-port-read → shell path.

### Timer monitor (`driver/timer.rs`)
Owns IRQ0, waits on the bridge, and logs every 100 notifications. Used to prove
no notifications are lost under continuous load: observed 100/100, 200/200,
300/300 with zero gaps.

---

## 5. Shell

New `irqs` command lists each IRQ line, its owning driver task, and the live
notification count, so the driver framework is observable at runtime.

---

---

## 6. MMIO and DMA foundation (`driver/mmio.rs`, `driver/dma.rs`)

### MMIO accessor layer
A new `Object::Mmio` capability variant models authority over physical memory
regions (e.g., device registers). The `driver/mmio` module provides
capability-checked read/write primitives at 8/16/32/64-bit widths:

- `mmio::read_u32(phys_addr)` / `write_u32(phys_addr, val)` etc.
- Checks for an `Object::Mmio` cap covering the address with `READ`/`WRITE` rights.
- Maps the physical region into the kernel's virtual address space (upper half)
  with `NO_CACHE | WRITE_THROUGH` flags to preserve device semantics.
- Idempotent: repeated accesses reuse the existing mapping.

### DMA pool allocator
`driver/dma` provides an 8 MiB physically-contiguous bump allocator for device
DMA buffers (command tables, scatter-gather lists, ring buffers). Modern devices
need physical addresses to program into their base-address registers; normal heap
allocations (`Box`, `Vec`) are virtually contiguous but not physically contiguous.

- `dma::alloc_dma(size)` returns `(virt_addr, phys_addr, rounded_size)`.
- Allocations are page-aligned and zeroed for security.
- Pool is mapped into kernel VA at a fixed offset (`DMA_POOL_VIRT_BASE`).

Drivers use the physical address to program devices and the virtual address for
CPU access (filling command structures, reading status).

---

## 7. PCI bus enumeration (`driver/pci.rs`)

The PCI (Peripheral Component Interconnect) bus is how x86 systems connect to
most hardware. The `pci` module walks bus 0 and discovers devices via legacy
I/O port access (CONFIG_ADDRESS=0xCF8, CONFIG_DATA=0xCFC):

- Reads vendor/device ID, class code, subclass, programming interface.
- Reads all 6 Base Address Registers (BARs) — MMIO/port ranges.
- `pci::find_device(predicate)` locates a specific device by vendor/class.
- Shell command `pci` / `lspci` lists all discovered devices.

Tested in QEMU: discovers 6–7 devices (host bridge, ISA bridge, VGA, E1000 NIC,
AHCI controller).

---

## 8. AHCI SATA driver (`driver/ahci.rs`)

A working proof-of-concept AHCI (Advanced Host Controller Interface) driver that
**detects and initializes SATA drives** on real/virtualized hardware:

### What works
- **PCI discovery:** finds the AHCI controller via class code 01:06:01.
- **BAR5 (ABAR) mapping:** reads the HBA memory base address, mints an
  `Object::Mmio` capability, maps the register space.
- **HBA reset and enable:** performs global reset, enables AHCI mode.
- **Port initialization (full sequence):**
  1. Stop the port (clear ST/FRE, wait for CR/FR to clear).
  2. Allocate DMA buffers for command list (1 KiB) and FIS receive area (256 bytes).
  3. Program the 64-bit base address registers (CLB/CLBU, FB/FBU).
  4. Clear SATA error and interrupt status registers.
  5. Spin up the device via COMRESET (SCTL DET=1 → 0).
  6. Wait for device ready (SSTS DET=3).
  7. Enable FIS receive (FRE) and command processing (ST).
  8. Read the signature register to identify device type (0x00000101 = SATA).

### Verification (QEMU with `-device ahci` + 2 attached drives)
```
[ahci] found controller at 00:04.0 — 8086:2922
[ahci] ABAR (BAR5) at phys 0xfebf1000
[ahci] HBA version 1.0, 6 ports max
[ahci] ports implemented: 0x0000003f
[ahci] port 0 — initializing (SSTS DET=0x3)
[ahci] port 0 — initialized: Sata (sig 0x00000101)
[ahci] port 1 — initialized: Sata (sig 0x00000101)
[ahci] initialization complete: 2 device(s) detected
```

Shell command `ahci` / `disks` lists detected SATA devices. DMA pool usage: 2.5 KiB
(two command lists + two FIS RX areas).

### What's next for AHCI
- **Interrupt handling** (IRQ notification when commands complete, instead of polling).
- Multiple outstanding commands (use all 32 command slots), NCQ.
- A freeable DMA allocator (the current bump pool leaks one command table per command).

---

## 9. AHCI command submission: IDENTIFY / READ / WRITE (`driver/ahci.rs`)

The driver now issues real ATA commands over a single, shared command path.

### Unified command path — `issue_command()`
All three commands (IDENTIFY, READ DMA EXT, WRITE DMA EXT) and FLUSH CACHE EXT go
through one helper that:
1. Waits for the port to be not-busy (`PORT_TFD` BSY/DRQ clear).
2. Allocates a 256-byte command table from the DMA pool.
3. Builds the command header in slot 0 of the command list.
4. Fills the H2D Register FIS (command, 48-bit LBA, sector count, LBA-mode device bit).
5. Builds one PRD entry pointing at the DMA data buffer (DBC = bytes − 1).
6. Clears stale `PORT_IS`, rings the doorbell (`PORT_CI` bit 0).
7. Polls `PORT_CI` for completion with an early `PORT_TFD` error check.

### Two important bug fixes made here
- **Command-header layout was wrong.** AHCI DW0 is `flags(16) | PRDTL(16)` and DW1 is
  a full 32-bit `PRDBC`. The previous struct packed PRDTL into bits 8–15 of the *low*
  half — which is actually the **Reset (R)** bit — and left the real PRDTL field zero.
  The HBA therefore saw "no PRD entries + reset" and every command timed out. The
  `CommandHeader` struct and the field writes were corrected.
- **DMA phys→virt mapping was off by the pool base.** The DMA pool's physical base is
  16 MiB, so the CPU virtual address of a buffer is `VIRT_BASE + (phys − PHYS_BASE)`,
  not `VIRT_BASE + phys`. A new `dma::phys_to_virt()` helper performs the correct
  inverse mapping; the device and the CPU now agree on the same physical page.

### Public API
- `ahci::identify_device(port)` → `[u8; 512]` raw identity; `parse_identify()` extracts
  model/serial (byte-swapped ATA strings, read without unaligned casts), 48-bit LBA
  sector count, and logical sector size.
- `ahci::read_sectors(port, lba, count, &mut buf)` — ATA READ DMA EXT (0x25).
- `ahci::write_sectors(port, lba, count, &buf)` — ATA WRITE DMA EXT (0x35) followed by
  FLUSH CACHE EXT (0xEA) for durability.

---

## 10. Block device abstraction layer (`driver/block.rs`)

A backend-agnostic storage layer so higher levels (a future file system) address
devices by stable name (`sata0`, `sata1`, …) instead of knowing about AHCI.

- **`BlockDevice` trait** (object-safe, `Send`): `name`, `block_size`, `block_count`,
  `read_blocks`, `write_blocks`, `capacity_bytes`.
- **`AhciBlockDevice`** wraps an AHCI SATA port, validates LBA ranges, and splits
  transfers larger than the 16-bit ATA sector-count field into chunks.
- **Global registry** with `register`, `list`, `read`, `write`, `find_by_name`,
  `device_count`.
- **`block::init()`** runs IDENTIFY on each SATA port and registers it as `sataN`.
- **`block::selftest()`** — non-destructive read/write round-trip: saves a block,
  writes a pattern, reads it back to verify, then restores the original.

### Shell commands
- `lsblk` / `blkdev` — list registered block devices with capacity.
- `blkread <dev> <lba>` — hex+ASCII dump of one block.
- `blkwrite <dev> <lba> <string>` — write a string into a block (zero-padded).

### Verification (QEMU, `-device ahci` + two 64 MiB drives, 4 clean boots)
```
[ahci] found controller at 00:04.0 — 8086:2922
[ahci] port 0 — initialized: Sata (sig 0x00000101)
[ahci] port 1 — initialized: Sata (sig 0x00000101)
[block] registered sata0 (131072 blocks x 512 bytes = 64 MiB)
[block] registered sata1 (131072 blocks x 512 bytes = 64 MiB)
[block] selftest PASSED on device 0 LBA 2048 (512-byte round-trip verified + restored)
```
The host disk image was confirmed to contain the written bytes during the test and
the original contents afterward, proving READ, WRITE, and FLUSH all reach the media.

---

## 11. NextFS: on-disk file system (`fs/nextfs.rs`)

**NextFS** is a simple, clean Rust-native file system designed specifically for PICKLE OS. It's a Unix-style filesystem with inodes and directories, built from scratch without ext2's legacy constraints.

### Design
- **Block size:** 512 bytes (matches standard SATA sector size).
- **Inodes:** 64-byte fixed structures (8 per block). Fields: type (file/dir), size, 12 direct block pointers, 1 single-indirect pointer.
- **Directories:** files whose data blocks hold fixed 64-byte records `[inode: u32, name: [u8; 60]]`. "." and ".." are explicit entries.
- **Root directory:** always inode 1 (inode 0 reserved).
- **Allocation:** first-fit bitmap for blocks and inodes.
- **Superblock:** block 0, stores magic `0x5346584E` ("NXFS"), block/inode counts, bitmap/inode-table/data start pointers.

### On-disk layout
```
Block 0:        Superblock
Block 1..N:     Free-block bitmap (1 bit per block)
Block N+1..M:   Inode table (8 inodes per 512-byte block)
Block M+1..:    Data blocks (file contents, indirect blocks)
```

### Operations implemented
- **Format:** `NextFS::format(dev_idx)` — writes fresh superblock, empty bitmaps, empty inode table, creates root directory with "." and "..".
- **Mount:** `NextFS::mount(dev_idx)` — validates magic, loads bitmaps into memory, scans inode table.
- **Directory ops:** `dir_lookup`, `dir_list`, `dir_add_entry`, `create_file`, `create_dir`.
- **File I/O:** `read_file`, `write_file`, `read_inode_data`, `write_inode_data` (handles block allocation, sparse files, indirect blocks).
- **Sync:** `sync()` — flushes superblock and bitmaps to disk.

### Shell integration
- `mkfs.nextfs <dev>` — format a block device.
- `mount <dev>` / `unmount` — mount/unmount the global filesystem.
- `nxls [path]`, `nxcat <path>`, `nxwrite <path> <text>`, `nxmkdir <path>` — file operations.

### Boot-time self-test (second disk, sata1)
```
[nextfs] === boot-time self-test ===
[nextfs] format: 131072 blocks, 1310 inodes, data @ block 197
[nextfs] format complete: root directory created
[nextfs] format: OK
[nextfs] mount: 131072 blocks, 1310 inodes, data @ 197
[nextfs] mount: OK
[nextfs] root dir: OK (2 entries)
[nextfs] create_dir /test: OK (inode 2)
[nextfs] create_file /test/hello.txt: OK (inode 3)
[nextfs] write_file: OK (40 bytes)
[nextfs] read_file: OK (verified 40 bytes)
[nextfs] /test dir: OK (3 entries)
[nextfs] sync: OK
[nextfs] === SELFTEST PASSED ===
```

The full create → write → read → verify cycle hits the real disk image; all data is persistent and correct.

### Current limitations
- No file deletion (would require free-list management for blocks/inodes).
- No timestamps, permissions, or hard links.
- Truncate leaks old blocks (write_file zeros size but doesn't free blocks).
- Single-indirect only (max file ~6 MiB with 512-byte blocks: 12 direct + 128 indirect pointers).
- No crash recovery (journaling / copy-on-write).

### What's next for NextFS
- Double/triple indirect for larger files.
- Free-block/inode reclamation (unlink, truncate).
- Timestamps, permissions, ownership metadata.
- Path resolution caching and directory entry caching.
- Integration with the VFS layer (currently separate from the IPC-based VFS server).

---

## 12. VFS layer and file syscalls (`fs/vfs.rs`, `syscall.rs`)

Now that NextFS provides persistent storage, we need a POSIX-like system call interface so user programs can open, read, write, and manipulate files.

### VFS shim layer

**Per-task file descriptor tables** (`fs/vfs.rs`):
- Each task gets its own FD table (stored in a global `BTreeMap<task_id, FdTable>`).
- FD table tracks open files with inode, flags (read/write/append), and current position.
- Standard descriptors 0-2 (stdin/stdout/stderr) reserved but not yet implemented.

**File operations**:
- `open(task_id, path, flags)` → resolves path, allocates inode, returns fd
- `read(task_id, fd, buf)` → reads from current position, updates file position
- `write(task_id, fd, buf)` → writes at current position, grows file if needed
- `seek(task_id, fd, offset, whence)` → repositions file offset (Set/Current/End)
- `close(task_id, fd)` → releases the file descriptor

**Path operations**:
- `readdir(path)` → lists directory entries
- `unlink(path)` → deletes a file
- `rmdir(path)` → removes an empty directory
- `mkdir(path)` → creates a directory
- `truncate(path, size)` → resizes a file, freeing blocks if shrinking

### File syscalls

Added 9 new syscalls (`syscall.rs`):
- `SYS_OPEN` (13): `(path_ptr, path_len, flags)` → fd
- `SYS_READ` (14): `(fd, buf_ptr, count)` → bytes_read
- `SYS_WRITE` (15): `(fd, buf_ptr, count)` → bytes_written
- `SYS_CLOSE` (16): `(fd)` → 0 or error
- `SYS_LSEEK` (17): `(fd, offset, whence)` → new_pos
- `SYS_UNLINK` (18): `(path_ptr, path_len)` → 0 or error
- `SYS_RMDIR` (19): `(path_ptr, path_len)` → 0 or error
- `SYS_MKDIR` (20): `(path_ptr, path_len)` → 0 or error
- `SYS_TRUNCATE` (21): `(path_ptr, path_len, size)` → 0 or error

All pointer arguments are validated for user-mode calls. Flags follow POSIX conventions (O_RDONLY=0, O_WRONLY=1, O_RDWR=2, O_CREAT=0x40, O_TRUNC=0x200, O_APPEND=0x400).

### NextFS enhancements

**Deletion support**:
- `unlink(dir_inode, name)` → removes file, frees all data blocks and inode
- `rmdir(parent_inode, name)` → removes empty directory (enforces "." and ".." only)
- `dir_remove_entry(dir_inode, name)` → marks directory entry as unused (inode=0)
- Protection: cannot delete "." or ".."

**Proper block reclamation**:
- `free_inode_blocks(inode)` → frees all direct and indirect blocks
- `truncate(inode, new_size)` → shrinks file, freeing blocks beyond new size
  - Handles direct blocks, single-indirect blocks, and the indirect block itself
  - `write_file` now calls `truncate` to free old blocks before rewriting

**New error variants**:
- `FsError::InvalidOperation` → tried to delete "." or ".."
- `FsError::DirectoryNotEmpty` → tried to rmdir a non-empty directory

### Shell commands

Added to the in-kernel shell (`shell.rs`):
- `nxrm <path>` → unlink a file
- `nxrmdir <path>` → remove an empty directory
- `nxtruncate <path> <size>` → resize a file to specified byte count

### Task lifecycle integration

- `spawn_kernel_task` and `spawn_user_task` now call `fs::init_task_fds(task_id)` to create the FD table.
- When a task exits, its FD table should be cleaned up (not yet implemented — would call `fs::cleanup_task_fds`).

### Boot-time self-test additions

Extended the NextFS self-test in `driver/mod.rs` to verify:
1. **Truncate**: shrinks a 40-byte file to 10 bytes, verifies new size
2. **File deletion**: creates temp.txt, writes data, unlinks it, verifies removal from directory
3. **Directory deletion**: creates a subdir, removes it via rmdir
4. **Syscall path**: mounts filesystem globally, opens/reads/closes via VFS syscalls

### Limitations

- No crash recovery if a task dies with open files (FD leak).
- No per-file locks or concurrent write protection.
- No fcntl, dup, pipe, or socket operations.
- User-mode tasks can access any file (no permission checks yet).
- Keyboard input via stdin not yet wired (returns EOF).

---

## 13. ELF loader from filesystem

Extended the existing ELF loader (`kernel/src/elf.rs`) to support loading executables from NextFS instead of only from embedded memory.

### Implementation

**New function: `elf::load_from_file(path)`**
- Opens the file via VFS layer (using task 0 context).
- Reads entire file into a buffer.
- Parses ELF headers and segments (reuses existing `ElfBinary::parse`).
- Loads segments into user-space memory (reuses existing `ElfBinary::load`).
- Returns entry point and stack pointer on success.

**Integration:**
- Uses VFS syscall-level API: `open()`, `seek()`, `read()`, `close()`.
- Supports arbitrary-sized binaries (dynamically allocates read buffer).
- Validates file is regular (not directory).
- Provides meaningful error messages ("file not found", "not a regular file", etc.).

### Benefits

- **No more embedded binaries required**: User programs can be stored on disk and loaded on demand.
- **Dynamic program loading**: Opens path to `exec()` syscall and user-space init systems.
- **Easier testing**: Recompile user program, write to NextFS, reload without rebuilding kernel.

---

## 14. Standard streams (stdin/stdout/stderr)

Implemented proper standard stream support in the VFS layer, allowing user programs to use file descriptor 0/1/2 for I/O instead of kernel-specific syscalls.

### VFS Layer Changes (`kernel/src/fs/vfs.rs`)

**1. File type abstraction:**
```rust
enum FileType {
    Regular(u32),  // NextFS inode
    Console,       // VGA + serial output
    Keyboard,      // Keyboard input
}
```

**2. OpenFile refactoring:**
- Changed from storing just `inode` to storing `FileType`.
- Added constructors: `OpenFile::console(writable)`, `OpenFile::keyboard()`.

**3. Per-task FD table initialization:**
- `init_task_fds()` now pre-populates:
  - **fd 0 (stdin)**: `Keyboard` type (read-only).
  - **fd 1 (stdout)**: `Console` type (write-only).
  - **fd 2 (stderr)**: `Console` type (write-only).

**4. I/O syscall dispatch:**

**`read(task_id, fd, buf)`:**
- `FileType::Regular(inode)` → read from NextFS.
- `FileType::Keyboard` → returns EOF for now (TODO: wire to shell's scancode queue).
- `FileType::Console` → error (write-only).

**`write(task_id, fd, buf)`:**
- `FileType::Regular(inode)` → write to NextFS.
- `FileType::Console` → output to VGA (`print!`) and serial (`serial_print!`).
- `FileType::Keyboard` → error (read-only).

**`seek(task_id, fd, offset, whence)`:**
- `FileType::Regular(inode)` → normal seeking.
- `FileType::Console | Keyboard` → error (`InvalidOperation`).

### User-space API

User programs can now use standard POSIX-like file descriptors:

```c
// Write to stdout (fd 1)
write(1, "Hello, world!\n", 14);

// Write to stderr (fd 2)  
write(2, "[error] Something failed\n", 25);

// Read from stdin (fd 0)
char buf[128];
read(0, buf, sizeof(buf));  // Currently returns EOF
```

### Test Program

Created `userspace/stdio_test.c`:
- Demonstrates writing to stdout and stderr via `SYS_WRITE`.
- Uses inline assembly syscall wrapper (no libc).
- Shows proper separation between stdout and stderr.
- Compiles to freestanding ELF binary.

### Benefits

- **Standard I/O interface**: User programs use familiar POSIX-style FDs instead of kernel-specific print syscalls.
- **Redirection-ready**: Stdout/stderr can later be redirected to files or pipes.
- **Cleaner separation**: Device I/O (console/keyboard) is handled separately from file I/O.
- **Foundation for pipes/shells**: Standard streams enable Unix-style pipelines and shell I/O redirection.

### Current Limitations

- No line buffering or editing (raw character mode only).
- Stdout and stderr both go to the same console (no separate error channel yet).
- `read()` on stdin is non-blocking: it returns however many characters are
  currently buffered (0 if none) rather than sleeping until input arrives.

> **Update (§17):** keyboard input is now fully wired to stdin through a shared
> console line discipline — `read(0, …)` returns typed characters.

---

## 15. Keyboard → stdin wiring (`driver/console.rs`)

Previously the `Keyboard` file type returned EOF because scancodes were consumed
directly by the in-kernel shell. We introduced a single **console line
discipline** that owns the one and only `pc_keyboard` decoder and a ring buffer
of decoded characters.

### Implementation

- **`kernel/src/driver/console.rs`** — the single decode point:
  - `feed_scancode(u8)` — called from the keyboard IRQ driver; decodes the
    scancode and pushes resulting `char`s into a buffer.
  - `read_char() -> Option<char>` — pops one decoded character (non-blocking).
  - `has_input() -> bool` — buffer-not-empty probe.
  - `inject_char(char)` — pushes a synthetic character (used by self-tests).
- **`driver/keyboard.rs`** now calls `console::feed_scancode()` instead of
  pushing scancodes onto a shell-private queue.
- **`shell.rs`** dropped its private scancode queue and `pc_keyboard` decoder; it
  reads through `console::read_char()`, so the shell and user-space stdin share
  exactly one decoder (no double-decode, no contention).
- **`fs/vfs.rs`** — the `FileType::Keyboard` read path drains
  `console::read_char()`, UTF-8 encodes the characters into the caller's buffer,
  and returns the number of bytes available (POSIX "return what's there").

### Verification

Boot-time self-test step 14 injects `"Pi"` via `console::inject_char()` and
reads it back through **fd 0 (stdin)** of task 0:

```
[nextfs] stdin read: OK (2 bytes: "Pi")
```

---

## 16. Permissions & ownership in NextFS (`fs/nextfs.rs`, `fs/vfs.rs`)

NextFS inodes now carry Unix-style metadata and the VFS enforces it.

### On-disk inode changes

The 64-byte inode gained `mode: u16`, `uid: u16`, `gid: u16` and `mtime: u32`.
To make room without growing the inode, the direct-pointer count was reduced
from 12 → **9** (single-indirect is unchanged, so max file size is
`(9 + 128) × 512 = 70,144` bytes ≈ 68 KiB). New files default to mode `0o644`,
directories to `0o755`.

### API

- `Inode::permits(uid, gid, want)` — classic owner/group/other rwx check; **uid 0
  (root) bypasses all checks**.
- `NextFS::stat / chmod / chown / set_owner_mode / check_permission`.
- `FileStat` with `is_dir()` and `mode_string() -> [u8;10]` (e.g. `-rw-r--r--`).
- VFS `Credentials { uid, gid }` per task (defaults to root); `credentials()` /
  `set_credentials()`. `open()` enforces `MAY_READ`/`MAY_WRITE`, the create path
  requires write on the parent directory and chowns the new file to the creator,
  and `unlink`/`rmdir`/`mkdir`/`truncate` require parent-directory write.
- New error: `FsError::PermissionDenied` → `VfsError::PermissionDenied`.

### Syscalls & shell

- `SYS_CHMOD (22)`, `SYS_CHOWN (23)`, `SYS_STAT (24)` (stat writes a 24-byte
  struct: type, mode, uid, gid, size, mtime).
- Shell commands `nxstat`, `nxchmod <octal> <path>`, `nxchown <uid> <gid> <path>`.

### Verification

Boot-time self-test step 11b creates a file owned by root mode `0o600` and
asserts: owner may read/write, a non-owner (uid 1000) is **denied**, widening to
`0o644` grants the non-owner read (but not write), and `chown` to uid 1000 grants
that user write:

```
[nextfs] permissions: OK (mode + owner enforcement verified)
```

---

## 17. User-space `init`: launching programs from disk (`init_user.rs`)

The kernel no longer hard-codes which user programs to run. After NextFS is
mounted globally, the AHCI bring-up task hands control to `init_user::run()`,
which behaves like a tiny `init`:

1. **Seed the root filesystem** — create `/bin` and `/etc`, then copy the
   embedded ELF images into `/bin/hello` and `/bin/test_lib` (the >4.6 KiB
   binaries also exercise the single-indirect block path).
2. **Write `/etc/inittab`** — a plain-text manifest, one `name:path` entry per
   line (`#` begins a comment).
3. **Parse `inittab` and launch each program straight from disk** via
   `task::spawn_user_task_from_file()`, which uses `elf::load_from_file()`.

Everything is defensive: a failure to seed or spawn any single program is logged
and skipped so the rest of boot proceeds. If fewer than two block devices are
present, NextFS is unavailable and `init` simply does not run.

### Build / run

`make run` now attaches two scratch SATA data disks (`disk0.img`, `disk1.img`,
created by the `disks` target) to an ICH9 AHCI controller, so NextFS mounts on
every normal boot.

### Verification

```
[init-user] === user-space init starting ===
[init-user] seed /bin/hello : OK (4912 bytes)
[init-user] seed /bin/test_lib : OK (4992 bytes)
[init-user] wrote /etc/inittab: OK
[init-user] spawned 'hello' from /bin/hello (task 12)
[user] Hello from ring 3!
[init-user] spawned 'test_lib' from /bin/test_lib (task 13)
[init-user] === user-space init complete (2 program(s) launched) ===
```

---

## 18. Bug fix: root-directory data block allocation collision (`fs/nextfs.rs`)

While bringing up disk-based `init`, a latent NextFS corruption bug surfaced.
`format()` allocated the root directory's data block (`data_start`) but never
marked it **used** in the block bitmap (a stale code comment claimed `mount()`
would handle it — it did not). On a freshly mounted filesystem the allocator
therefore handed out `data_start` again for the very first `alloc_block()`,
overwriting the root directory's contents with those of the first subdirectory.

The bug was masked because the previous self-test never re-listed `/` after
creating a subdirectory, and default boots had no second disk so NextFS never
mounted. The disk-based `init` (which re-mounts and resolves `/bin/...`) exposed
it immediately as a spurious `NotFound`.

**Fix:** `format()` now clears the `data_start` bit in the block bitmap (marking
it used) and accounts for it in `free_blocks`. After the fix, a remounted root
directory correctly resolves its children and all syscall/`init` paths succeed.

---

## 19. What's next in Phase 3

- **Interrupt-driven AHCI:** command-completion IRQs instead of polling; multiple command slots / NCQ.
- **Promote drivers to ring-3:** fully isolated user-space servers with MMIO/DMA caps.
- **Driver reuse investigation:** DDE / rump-kernel-style shims for NIC/Wi-Fi drivers.
- **Network stack:** TCP/IP over a real NIC driver (E1000 or virtio-net).
