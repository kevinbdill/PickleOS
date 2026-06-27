# PICKLE OS Roadmap

PICKLE OS aims to become a modern, capability-secured desktop operating system with a
pure-Rust userland, user-space drivers, and a Wayland compositor backed by Vulkan.
That is a multi-year effort. This roadmap is honest about the gap between **what
boots today** and that north star, and orders the work so each phase produces
something runnable.

The target hardware class is refurbished **2018–2022 Intel Core i5/i7** laptops and
desktops — common, cheap, well-documented, and with mature open driver knowledge.

---

## Phase 0 — Microkernel foundation ✅ (this repository)

A real, bootable microkernel with the mechanisms everything else depends on.

- [x] BIOS boot via `bootloader` 0.9, long mode, physical-memory map
- [x] Serial + VGA consoles
- [x] GDT/TSS with IST stacks and per-task `rsp0`
- [x] IDT, CPU exceptions, double-fault on IST
- [x] PIC remap, PIT timer tick, PS/2 keyboard
- [x] 4-level paging, boot-info frame allocator, region mapping
- [x] Kernel heap (`alloc` enabled)
- [x] Preemptive scheduler with real context switching
- [x] Kernel tasks: spawn / yield / sleep / block / unblock / exit
- [x] Syscall ABI via `int 0x80`
- [x] Synchronous IPC (send/receive/call/reply, named endpoints)
- [x] Capability tables and rights
- [x] Interactive in-kernel shell
- [x] Ring-3 *infrastructure* (user segments, syscall DPL=3)

**Exit criterion (met):** kernel boots in QEMU and runs multiple preemptively
scheduled tasks exchanging IPC messages.

---

## Phase 1 — User-space processes ✅

Turn the ring-3 infrastructure into real, isolated processes.

- [x] Per-process address spaces (separate page tables, switch CR3 on dispatch)
- [x] ELF loader for static user binaries
- [x] User stacks and ring-3 entry via iretq
- [x] User-mode task creation (spawn_user_task)
- [x] First "hello world" user program that makes syscalls
- [x] Syscall boundary validation (user pointer checks)

**Exit criterion (met):** a separately compiled user ELF (`hello`) runs in ring 3,
isolated in its own address space, and successfully makes syscalls (print, getpid, exit).
The user task exits cleanly after printing messages via the kernel.

---

## Phase 2 — Core OS services ✅

The services a usable system needs, as discrete tasks that communicate purely
over IPC and named endpoints. (They currently run as in-kernel tasks using the
user-facing IPC API — see `docs/PHASE2_STATUS.md` for why, and the small
marshalling change needed to relocate them into isolated address spaces.)

- [x] A root/`init` server that bootstraps the service stack and the namespace
- [x] A virtual file system (VFS) server with a pluggable backend
- [x] An in-memory file system (MemFS) backend
- [x] A registry server for naming and discovery
- [x] `libpickleos`: a libc-free std-like crate for user programs
- [x] A shell with coreutils-style commands (`ls`, `cat`, `write`, `touch`,
      `mkdir`, `rm`, `stat`, `services`) routed through the VFS over IPC
- [x] Boot self-test proving the `shell → VFS → MemFS` path end-to-end (headless)

**Exit criterion:** boot reaches a shell that can list and read files served by
the VFS. **Met** (see `docs/PHASE2_STATUS.md`).

**Carried into Phase 3:** relocating these services into isolated ring-3 address
spaces (needs page-table deep-copy) and dynamic `SYS_SPAWN` loading from the VFS.

---

## Phase 3 — Drivers in user space (incl. reuse)

Drivers are unprivileged processes granted exactly the MMIO/IRQ/DMA capabilities
they need.

- [x] **User-space driver framework foundation** (see `PHASE3_STATUS.md`):
      IRQ delivery over a lock-free ISR→task bridge, capability-checked port I/O
      (`Object::Port`), and two reference driver tasks (PS/2 keyboard on IRQ1 +
      port 0x60, timer monitor on IRQ0). Validated end-to-end.
- [x] **Per-task address-space isolation correctness**: each user task runs in its
      own CR3, reloaded on every context switch (not just first run) — eliminated
      an intermittent cross-address-space corruption/page-fault race.
- [x] **MMIO + DMA + PCI plumbing**: capability-checked MMIO accessors
      (`Object::Mmio`), an 8 MiB physically-contiguous DMA pool, and PCI bus
      enumeration — the substrate every bus-mastering driver needs.
- [x] **AHCI SATA block driver**: controller discovery, port init, IDENTIFY DEVICE,
      and **READ/WRITE DMA EXT (48-bit LBA) + FLUSH CACHE EXT**, all over a unified
      command-submission path. Verified by a non-destructive on-disk read/write
      round-trip across multiple clean boots (see `PHASE3_STATUS.md` §9).
- [x] **Block device abstraction layer** (`BlockDevice` trait + registry): SATA
      ports exposed as `sataN`; backend-agnostic seam for NVMe/RAM disks and the
      future file system. Shell: `lsblk`, `blkread`, `blkwrite`.
- [x] **NextFS: on-disk file system** (`fs/nextfs.rs`): Rust-native Unix-style FS
      with inodes (64 bytes, 8 per 512-byte block), directories (fixed 64-byte entries),
      12 direct + 1 indirect block pointers per inode. Operations: format, mount, mkdir,
      create/read/write files, directory listing. Shell: `mkfs.nextfs`, `mount`, `nxls`,
      `nxcat`, `nxwrite`, `nxmkdir`. Boot-time self-test PASSED (format → mount → create
      dir + file → write 40 bytes → read back & verify → sync to disk).
- [x] **File deletion & truncate** (`fs/nextfs.rs`): proper block reclamation with `unlink`,
      `rmdir`, and `truncate` operations. Frees direct blocks, indirect blocks, and inodes.
      Shell commands: `nxrm`, `nxrmdir`, `nxtruncate`.
- [x] **VFS layer + file syscalls** (`fs/vfs.rs`, `syscall.rs`): per-task file descriptor
      tables, POSIX-like file operations (open/read/write/close/lseek), and 9 new syscalls
      (SYS_OPEN through SYS_TRUNCATE). Path resolution, directory operations (readdir/mkdir/
      rmdir/unlink), and integration with task lifecycle.
- [ ] NVMe block driver (same `BlockDevice` interface)
- [ ] Interrupt-driven AHCI completion (replace polling); multiple command slots / NCQ
- [ ] Intel HD/UHD Graphics (Gen9–Gen12) KMS/display driver — the i5/i7 target GPUs
- [ ] **Driver reuse** investigation: DDE / rump-kernel-style shims to run existing
      (Linux/NetBSD) driver code in a confined user-space server, to bootstrap NIC/Wi-Fi
      and exotic hardware without writing every driver from scratch
- [ ] Networking stack (user space) over a real NIC driver

**Exit criterion:** persistent storage on real hardware and at least one network
interface, all driven from user space.

---

## Phase 4 — Graphics & desktop

The modern GUI stack.

- [ ] Vulkan-capable GPU driver path on Intel Gen9–Gen12
- [ ] A **Wayland compositor** using retained-mode Vulkan rendering
- [ ] Input routing (keyboard/mouse/touchpad) to clients
- [ ] A toolkit and a couple of native Rust GUI apps (terminal, file manager)
- [ ] Multi-monitor, DPI scaling

**Exit criterion:** a graphical desktop session with Wayland clients on target hardware.

---

## Phase 5 — Hardening & breadth

- [ ] SMP (APIC, per-CPU run queues, locking review)
- [ ] UEFI boot path alongside BIOS
- [ ] Power management (ACPI, sleep states)
- [ ] Audio, USB stack, Bluetooth
- [ ] Scheduler with priorities / fairness / accounting
- [ ] Security review of the capability model end-to-end
- [ ] Self-hosting: build PICKLE OS on PICKLE OS

---

## Guiding principles

1. **Capabilities everywhere** — no ambient authority; every privilege is an explicit,
   delegable, revocable capability.
2. **Keep the kernel small** — if it can run in user space, it should.
3. **Each phase boots** — never a year-long refactor with nothing to run.
4. **Reuse pragmatically** — writing every driver from scratch is infeasible; lean on
   DDE/rump-style reuse where it buys real hardware support.
5. **Rust end to end** — memory-safe userland, not a GNU/POSIX clone.
