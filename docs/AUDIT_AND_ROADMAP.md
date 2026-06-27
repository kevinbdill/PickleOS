# PICKLE OS — Comprehensive Codebase Audit & Forward Roadmap

> **Status:** Read-only audit. No code was modified to produce this document.
> **Date:** 2026-06-26
> **Scope:** Full kernel + userland audit (≈13,400 lines of Rust across 38 modules,
> plus the C userspace), followed by a gap analysis and a multi-phase roadmap to
> evolve PICKLE OS into a GUI-centric OS with a desktop environment, dynamic
> linking, full process management/IPC, a TCP/IP stack, wired NIC drivers, and
> 802.11/WPA2 WiFi.

---

## 1. Executive Summary

PICKLE OS is a genuinely working, memory-safe x86_64 OS written in Rust. It boots
via BIOS (bootloader 0.11), brings up a linear framebuffer, runs a preemptive
multitasking scheduler with real context switching, isolates user processes in
ring 3 with per-process page tables, persists a custom on-disk filesystem
(NextFS), enforces UID/GID/mode permissions, and presents a compositing window
manager with draggable windows and live terminals. Process management
(`fork`/`exec`/`wait`/`exit`/`pipe`) is implemented and exercised by user
programs.

It calls itself a *microkernel*, and the **mechanisms** for that vision exist
(capabilities, IPC, an IRQ→task bridge, capability-checked MMIO/port/DMA). But in
the current build the **policy** is still monolithic: drivers, filesystems, the
window manager, and the "user-space" services all run as in-kernel tasks sharing
the kernel address space. That is a reasonable, honest staging point — and most
importantly, *it boots and runs*.

To reach the stated north star (advanced desktop, dynamic linking, networking,
WiFi), the project needs work in roughly this dependency order:

1. **Foundational debt** (heap size, frame reclamation, demand paging, RTC,
   APIC) — these block almost everything else and should be paid down first.
2. **Process model completion** (argv/envp, signals, COW fork, dynamic spawn).
3. **Dynamic linking** (PIE/`ET_DYN`, a dynamic loader, shared objects).
4. **A real display-server architecture** (client/server protocol, shared-memory
   surfaces, a widget toolkit) — the current compositor is kernel-resident and
   cannot host third-party apps.
5. **A driver framework** capable of bus-mastering DMA NICs + MSI/MSI-X.
6. **A TCP/IP stack** (recommend `smoltcp`) over an **E1000/RTL8139** driver.
7. **WiFi** (by far the largest single effort: 802.11 MLME + WPA2 supplicant +
   per-chip firmware loading).

The single highest-leverage near-term fix is **growing the kernel heap and adding
physical-frame reclamation** — the current 1 MiB heap and never-reuse frame
allocator cap nearly every ambitious feature below.

---

## 2. Audit Methodology

- Enumerated every kernel module (`kernel/src/**/*.rs`) and measured size/scope.
- Read the boot path end-to-end (`main.rs` → `init::init_all` → task spawns →
  `driver::start` → `services::start` → `gui::compositor_task`).
- Traced each target capability (GUI, dynlink, IPC, net, WiFi) from the syscall
  ABI down to the hardware accessors to find exactly where each one stops.
- Cross-referenced the existing `docs/ROADMAP.md`, `PHASE2/3_STATUS.md`, and
  `PHASE2_INTERRUPT_ATTEMPT.md` so this document extends rather than contradicts
  prior planning.

### Module inventory (by size)

| Module | LOC | Responsibility | Maturity |
|---|---:|---|---|
| `fs/nextfs.rs` | 1357 | On-disk filesystem (inodes, bitmap, dirs) | Solid, limited |
| `shell.rs` | 1354 | In-kernel shell + pipe/redirect | Solid |
| `task.rs` | 1117 | Tasks, scheduler, fork/exec/wait/exit | Solid, gaps |
| `fs/vfs.rs` | 982 | FD tables, file ops, pipes | Solid |
| `driver/ahci.rs` | 802 | SATA AHCI (polling) | Works (polling) |
| `syscall.rs` | 712 | Syscall ABI + dispatch (29 calls) | Solid |
| `gui.rs` | 621 | Compositing window manager | Works, kernel-resident |
| `terminal.rs` | 577 | Text-grid terminal model | Solid |
| `framebuffer.rs` | 519 | Linear FB pixel ops + text console | Solid, no 2D accel |
| `driver/mod.rs` | 420 | Driver bring-up + NextFS self-test | Solid |
| `memory.rs` | 403 | Paging, frame alloc, addr-space dup | Works, no reclaim/COW |
| `elf.rs` | 381 | ELF loader (`ET_EXEC` static only) | Works, static-only |
| `driver/mouse.rs` | 232 | PS/2 mouse | Works |
| `interrupts.rs` | 232 | IDT, exceptions, PIC IRQs | Works (PIC, no APIC) |
| `ipc.rs` | 242 | Synchronous message passing | Works, fixed-size msgs |
| `services/*` | ~600 | init/registry/vfs/memfs (in-kernel) | Demo-grade |
| `capability.rs` | 212 | Capability tables + rights | Solid |
| `driver/mmio.rs` | 212 | Capability-checked MMIO | Solid |
| `driver/pci.rs` | 202 | PCI bus-0 enumeration | Works, bus-0 only |
| `driver/dma.rs` | 200 | Contiguous DMA pool | Works |
| `driver/block.rs` | 285 | `BlockDevice` trait + registry | Solid |
| `driver/console.rs` | 182 | Line discipline / stdin ring buffer | Solid |
| `driver/irq.rs` | 131 | IRQ→task notification bridge | Solid |
| `driver/keyboard.rs` | 130 | PS/2 keyboard | Works |
| **networking** | **0** | — | **Absent** |
| **dynamic linker** | **0** | — | **Absent** |

---

## 3. Current State Analysis — What Exists and Works

### 3.1 Kernel core & boot

- **Boot:** BIOS via `bootloader_api` 0.11 with `Mapping::Dynamic` physical-memory
  mapping and a requested ≥1024×768 framebuffer. Entry is `kernel_main`.
- **CPU structures:** GDT + TSS with an IST stack for double faults; IDT with CPU
  exception handlers, PIC IRQs (timer/keyboard/mouse/AHCI), and an `int 0x80`
  syscall gate at DPL=3.
- **Memory:** 4-level paging via `OffsetPageTable`; a `BootInfoFrameAllocator`
  seeded from the memory map; a kernel heap behind `linked_list_allocator`.
- **Scheduling:** preemptive, timer-driven (PIT ~18.2 Hz), real `context_switch`
  in assembly, per-task 128 KiB heap-backed kernel stacks. States:
  Runnable/Running/Blocked/Zombie/Dead.
- **IPC:** synchronous `send`/`receive`/`call`/`reply` with named endpoints;
  fixed `Message` of `{tag, [u64;6], sender}`.
- **Capabilities:** per-task tables of `{Object, Rights}` with monotonic
  rights-on-derivation; objects cover Endpoint/Memory/Irq/Port/Mmio.

### 3.2 Process management (implemented)

- `do_fork` — deep-copies the parent's user address space
  (`memory::duplicate_user_address_space`, full byte-copy of every
  `USER_ACCESSIBLE` page — **no copy-on-write**), clones FD table, crafts a child
  kernel stack returning 0 from the syscall.
- `do_exec` — loads a fresh ELF from NextFS, preserves PID/parent/children/FDs,
  rewrites the syscall return frame to enter the new entry point. **Now accepts
  `argv`/`envp`**: the loader (`elf::setup_user_stack`) lays out a System V
  x86-64 startup stack (argc, argv[], NULL, envp[], NULL, AT_NULL auxv, strings)
  so programs receive arguments and environment. `exec` also resets caught
  signal handlers to their defaults.
- `do_wait` — blocks for *any* child to become a zombie, reaps it, optionally
  writes status. (No `waitpid(pid)`, no `WNOHANG`/options.)
- `do_exit` — zombifies, closes FDs, reparents orphans to init (PID 1), wakes a
  waiting parent (and raises `SIGCHLD` on it). A reaper task adopts/reaps true
  orphans.
- `pipe()` — VFS-backed 4 KiB circular buffer with blocking semantics,
  EOF/broken-pipe handling, refcounted ends, fork/close integration.
- **Identity**: `getpid` / `getppid` report a task's own and parent PIDs.
- **Signals** (`kernel/src/signal.rs`) — per-task `SignalState` (pending bitmap +
  handler table + restorer trampoline + saved context). `kill(pid, sig)` applies
  the disposition: `SIGKILL` always terminates (uncatchable); default actions
  terminate (exit status `128 + sig`) except `SIGCHLD` which is ignored;
  `SIG_IGN` discards; a registered handler is marked pending and delivered at the
  target's next syscall-return boundary by `deliver_pending_signals` (which
  rewrites the trap frame to enter the handler with the signal number in `rdi`).
  `signal(sig, handler, restorer)` installs a disposition; the handler returns
  through a user trampoline that issues `SYS_SIGRETURN`, restoring the
  interrupted context via `do_sigreturn`. `fork` inherits handlers (clearing
  pending); `exec` resets them. Supported signals: `SIGHUP`, `SIGINT`,
  `SIGKILL`, `SIGUSR1`, `SIGUSR2`, `SIGTERM`, `SIGCHLD`.

### 3.3 Syscall surface (33 calls)

`PRINT, GETPID, EXIT, YIELD, TICKS, IPC_SEND, IPC_RECV, CAP_CHECK, SLEEP, SPAWN*,
MMAP, MUNMAP, OPEN, READ, WRITE, CLOSE, LSEEK, UNLINK, RMDIR, MKDIR, TRUNCATE,
CHMOD, CHOWN, STAT, EXIT2, WAIT, EXEC, FORK, PIPE, GETPPID, KILL, SIGNAL,
SIGRETURN`.
(`SPAWN` is a stub — dynamic loading from a user pointer is not implemented.
`EXEC` now takes `(path, len, argv, envp)`, reading `envp` from `r10`.)

### 3.4 Filesystem & VFS

- **NextFS:** 512-byte blocks; 64-byte inodes (8/block); superblock + free-block
  bitmap + inode table + data region; directories of fixed 64-byte entries; **9
  direct + 1 single-indirect** pointers ⇒ **~68 KiB max file size**. Supports
  format/mount/persistence, create/read/write/truncate, mkdir/rmdir/unlink, and
  UID/GID/mode/mtime with POSIX-style permission checks.
- **VFS:** per-task FD tables, `open/read/write/close/lseek`, path resolution,
  `readdir/mkdir/rmdir/unlink/stat`, STDIN/STDOUT/STDERR, and pipes.
- **Persistence:** boot mounts an existing volume; only formats a blank disk.

### 3.5 Drivers & interrupts

- **PCI:** programmatic config-space access; enumerates **bus 0 only** (no bridge
  recursion, no PCIe ECAM, no MSI/MSI-X).
- **AHCI:** discovery, port init, IDENTIFY, READ/WRITE DMA EXT + FLUSH — **polling**
  (interrupt path hangs writing `PORT_IE`; documented and reverted).
- **Block layer:** `BlockDevice` trait + `sataN` registry.
- **Input:** PS/2 keyboard (IRQ1) and mouse (IRQ12) as driver tasks via the
  IRQ→task bridge; a central line discipline feeds STDIN.
- **DMA:** 8 MiB physically-contiguous pool. **MMIO/Port/DMA all capability-gated.**

### 3.6 Graphics / GUI

- **Framebuffer:** direct linear pixel access (`put_pixel`/`fill_rect`/`clear`/
  `read_pixel`), an 8×8 bitmap-font text console, RGB/BGR/U8 formats. **No back
  buffer**, no 2D acceleration, no alpha/clipping beyond rectangles.
- **Compositor (`gui.rs`):** a single kernel task that owns the screen, draws a
  desktop + taskbar + draggable/focusable windows, a software mouse cursor with
  per-pixel save-under, and renders live terminal grids. Window kinds are limited
  to `Info` (static text) and `Terminal`.

---

## 4. Gap Analysis — What's Missing

### 4.0 Cross-cutting foundational debt (blocks most features)

| Gap | Impact | Where |
|---|---|---|
| **Kernel heap ~1 MiB** | Can't allocate a full-screen back buffer (~2.7 MiB), large net buffers, or firmware blobs. Explicitly noted as the reason the compositor has no back buffer. | `allocator.rs` |
| **No physical-frame reclamation** | `BootInfoFrameAllocator` never reuses frames; every `fork`/`exec`/page free leaks RAM. Long-running net/GUI workloads will exhaust memory. | `memory.rs` |
| **No demand paging / COW** | Page faults are fatal (`panic!`). `fork` byte-copies the whole address space; no lazy stacks, no `mmap` file backing, no guard pages on kernel stacks. | `interrupts.rs`, `memory.rs` |
| **PIC only, no APIC/IOAPIC** | No MSI/MSI-X (modern NICs/NVMe prefer/require it), no SMP, coarse 18.2 Hz timer. | `interrupts.rs` |
| **No RTC / wall-clock time** | NextFS mtime uses tick count; TLS/cert validation and TCP timestamps need real time. | `fs/nextfs.rs` |
| **Single CPU** | No SMP; throughput-bound for compositor + netstack + apps. | `task.rs` |

### 4.1 Advanced GUI desktop environment

The current compositor is **kernel-resident and monolithic** — third-party apps
cannot draw. Missing:
- A **display-server/client protocol** (apps submit surfaces; server composites).
- **Shared-memory surfaces** (apps render into a buffer the server maps).
- **Double buffering / damage tracking / clipping** for flicker-free compositing.
- A **widget toolkit** (buttons, text fields, lists, menus, layout).
- **Drawing API for apps** (2D canvas: lines, polygons, blits, alpha blending).
- **Font rendering beyond 8×8 bitmap** (anti-aliased/TrueType).
- **Image/icon decoding** (PNG/BMP), a cursor theme, wallpaper.
- **Window decorations, resize, minimize/maximize, z-order beyond a Vec push.**
- **Input routing to focused client** with event delivery over IPC.
- **GPU acceleration** (Intel KMS / Vulkan path is a long-term north-star item).
- **Clipboard, drag-and-drop, multi-monitor, DPI scaling.**

### 4.2 Dynamic linking & shared libraries

**Entirely absent.** The loader handles only `ET_EXEC`, statically linked at a
fixed `0x400000` (`user.ld`). Missing:
- **PIE / `ET_DYN`** support and load-bias relocation.
- A **dynamic loader/interpreter** (`ld.so` equivalent) — `PT_INTERP`,
  `PT_DYNAMIC` parsing.
- **Relocation processing** (`R_X86_64_RELATIVE`, `GLOB_DAT`, `JUMP_SLOT`),
  **GOT/PLT** setup, lazy binding.
- **Shared object (`.so`) format**, symbol tables/hash, versioning.
- **`dlopen`/`dlsym`** runtime API; per-process link maps.
- A **userland C library or Rust `std`-like runtime** rich enough to be worth
  sharing (today `libpickleos` is a static `rlib`).

### 4.3 Full process management & IPC

| Missing | Notes |
|---|---|
| ~~**argv/envp passing**~~ | ✅ **Done.** `exec(path, len, argv, envp)` lays out a System V startup stack; see §3.2 and `/bin/args_test`. |
| ~~**Signals**~~ | ✅ **Done** (basic). `kill`/`signal`/`sigreturn`, default + caught dispositions, `SIGCHLD` on child exit; delivered at the syscall boundary. No `SIGSEGV`→userspace (page faults still panic), no `sigaction` flags/masks/queued siginfo. See §3.2 and `/bin/signal_test`. |
| ~~**`getppid`**~~ | ✅ **Done.** See `/bin/pid_test`. |
| **`waitpid` options** | No specific-PID wait, no `WNOHANG`. |
| **Threads** | One thread per address space; no `clone`/TLS/futex. |
| **Process groups / sessions / job control** | None. |
| **Dynamic `spawn` from VFS** | `SYS_SPAWN` is a stub. |
| **IPC: bulk/shared-memory transfer** | `Message` is 6 words; no grant/map of memory across tasks despite `Object::Memory` existing. |
| **IPC: capability passing** | Can't send a capability in a message. |
| **IPC: async notifications / select/poll** | Only blocking recv + the IRQ bridge. |
| **Services in real address spaces** | init/registry/vfs/memfs are in-kernel tasks. |

### 4.4 TCP/IP networking stack

**Entirely absent** — no sockets, no protocols, no loopback. Needs: link layer
(Ethernet framing), ARP, IPv4 (and ideally IPv6), ICMP, UDP, TCP, DHCP client,
DNS resolver, a **socket syscall/IPC API**, and a routing table. (Strong
recommendation: adopt `smoltcp` rather than hand-rolling.)

### 4.5 Network drivers (E1000, RTL8139)

**Absent.** PCI enumeration exists but there is **no NIC driver, no MMIO BAR
mapping for NICs, no DMA descriptor rings, no MAC/link bring-up**. Both target
chips need: BAR0 MMIO mapping, reset/EEPROM/MAC read, TX/RX descriptor rings in
DMA memory, interrupt (or polled) RX, and a `NetDevice` trait analogous to
`BlockDevice`. RTL8139 is the simpler first target; E1000 (82540EM, QEMU default)
is well-documented and DMA-ring based.

### 4.6 WiFi (802.11 / WPA2 / firmware)

**Absent and by far the largest effort.** Requires:
- A **PCIe WiFi NIC driver** (e.g. Intel iwlwifi-class) — **firmware blob loading**
  from disk into device memory, DMA command/response queues, MSI-X.
- **802.11 MAC (MLME):** scanning, authentication, association, management frames.
- **WPA2 supplicant:** 4-way handshake, EAPOL, AES-CCMP, PBKDF2/SAE crypto.
- **Crypto primitives** (AES, SHA, HMAC) — needs the RTC/entropy story too.
- **Regulatory/channel handling**, rate control, power save.
- Integration so the netstack sees WiFi as just another `NetDevice`.

---

## 5. Architecture Recommendations

### 5.1 GUI subsystem — move to a display-server model

**Target:** a `displayd` user-space server that owns the framebuffer (granted an
`Object::Mmio` capability over the FB region) and composites client surfaces.

```
+-------------------+        IPC (endpoints)        +------------------------+
|  App (ring 3)     |  --- create_surface() ----->  |  displayd (ring 3)     |
|  draws into a     |  --- attach SHM buffer --->   |  - owns framebuffer cap|
|  shared-mem buffer|  <-- input events ---------    |  - z-order, damage     |
+-------------------+                                |  - composites -> FB    |
        ^   shared memory surface (Object::Memory)   +------------------------+
        +--------------------------------------------------------+
```

Recommended build order:
1. **Shared-memory IPC first** (prerequisite): let a task grant a memory region
   to another (`Object::Memory` + a `map_shared` syscall). This unblocks both GUI
   surfaces and bulk net transfer.
2. **Back buffer + damage tracking** in the compositor (needs the bigger heap).
3. **Surface protocol**: create/resize/commit/destroy + damage rects over IPC.
4. **Input routing**: deliver keyboard/mouse events to the focused client's
   endpoint instead of the hard-wired terminal.
5. **Rust widget toolkit** (immediate-mode is simplest in `no_std`): buttons,
   labels, text input, lists; software-rendered.
6. **Anti-aliased fonts** (`ab_glyph`/`fontdue` are `no_std`-friendly) and PNG
   decode (`tinybmp`/a small PNG crate) for icons/wallpaper.
7. Long-term: relocate `displayd` to its own address space; later a GPU/KMS path.

> Pragmatic note: the existing `gui.rs` can remain the *bootstrap* compositor while
> `displayd` is built beside it; cut over once the protocol works.

### 5.2 Networking stack — adopt `smoltcp` over a `NetDevice` trait

**Do not hand-roll TCP.** `smoltcp` is a mature, `no_std`, allocation-light
Rust TCP/IP stack used by other hobby/embedded OSes. Plan:
1. Define a **`NetDevice` trait** (mirroring `BlockDevice`): `transmit(frame)`,
   `receive() -> Option<frame>`, `mac()`, `link_up()`, plus a poll/IRQ hook.
2. Write the NIC driver (§5.3) implementing `NetDevice`.
3. Wrap the driver in `smoltcp`'s `phy::Device` interface; run a **`netd`**
   task that owns the device and runs the stack's poll loop.
4. Expose a **socket API** to apps: either BSD-style `socket/bind/connect/
   send/recv` syscalls, or (more microkernel-idiomatic) an IPC protocol to
   `netd` with per-socket endpoints. Recommend a thin syscall facade over IPC.
5. Add **DHCP** (smoltcp has it) and a small **DNS** resolver.
6. Provide **loopback** early for testing without hardware.

### 5.3 Driver framework — generalize the existing seams

The block-device pattern is the template. Generalize it:
- **`NetDevice` / `GpuDevice` / `InputDevice` traits** + registries, like
  `BlockDevice`.
- **MSI/MSI-X** support (requires APIC/IOAPIC — see §4.0) for modern NICs; fall
  back to legacy INTx (already wired via the IRQ bridge) for RTL8139/E1000.
- **A DMA-ring helper** in `driver/dma.rs` (allocate ring + buffers, virt↔phys
  helpers) reused by NIC and NVMe drivers.
- **PCI improvements:** multi-bus/bridge recursion, BAR sizing (write-1s probe),
  enabling bus-mastering (`Command` register bit 2), capability list walking
  (for MSI/MSI-X). These are small, high-value changes.
- **Firmware loading service** (for WiFi): read blobs from NextFS and DMA them in.
- Keep the capability model: each driver gets exactly the `Mmio`/`Irq`/`Port`/DMA
  caps it needs.

---

## 6. Multi-Phase Roadmap

Each phase is sized in relative **complexity (S/M/L/XL)** and lists hard
**dependencies**. Phases are ordered so each one boots and demonstrates something.
This extends the existing `docs/ROADMAP.md` (Phases 0–3 are largely done there).

### Phase A — Foundational debt paydown  *(complexity: L)*
**Goal:** remove the ceilings that block everything below.
- Grow the kernel heap (and make it growable) — target ≥16 MiB.
- Add a **physical frame free-list** so `fork`/`exec`/unmap reclaim RAM.
- **Demand paging + COW**: handle page faults (lazy stack growth, COW fork,
  file-backed `mmap`); add kernel-stack guard pages.
- Bring up **Local APIC + IOAPIC** (keep PIC fallback); higher-res timer.
- Add an **RTC/wall-clock** source (CMOS RTC + TSC calibration).
- **Dependencies:** none (do first).
- **Unblocks:** GUI back buffer, COW fork, MSI, net buffers, TLS time.

### Phase B — Process model & IPC completion  *(complexity: M)*
**Goal:** a POSIX-ish process/IPC base apps can rely on.
- `exec` with **argv/envp**; an ABI for the initial stack.
- **Signals** (at least `SIGCHLD`, `SIGKILL`, `SIGSEGV`→handler) .
- `waitpid(pid, options)`; `getppid`.
- **Shared-memory IPC** (`map_shared`/grant `Object::Memory`) + **capability
  passing** in messages; an async **notification**/`poll` primitive.
- Implement dynamic **`SYS_SPAWN`** loading an ELF from the VFS.
- (Stretch) **threads** (`clone`, TLS, a futex).
- **Dependencies:** A (COW makes fork cheap; not strictly required).
- **Unblocks:** GUI surfaces (shared mem), netd socket IPC, shells with args.

### Phase C — Dynamic linking & shared libraries  *(complexity: L)*
**Goal:** load PIE executables and `.so` libraries.
- Loader: parse `PT_INTERP`/`PT_DYNAMIC`, support **`ET_DYN`** + load bias.
- A **dynamic linker** (in userland): relocations (`RELATIVE/GLOB_DAT/JUMP_SLOT`),
  GOT/PLT, symbol resolution, lazy binding.
- Define the **shared-object ABI**; build `libpickleos` as a shared library.
- `dlopen`/`dlsym`; per-process link maps.
- **Dependencies:** B (argv/env, spawn-from-VFS), A (mmap for mapping segments).
- **Unblocks:** third-party apps that share a runtime; smaller binaries.

### Phase D — Display server & desktop environment  *(complexity: XL)*
**Goal:** apps render their own windows; a real desktop.
- `displayd` server: framebuffer cap, **back buffer + damage + clipping**.
- **Surface protocol** over IPC; **shared-memory surfaces**.
- **Input routing** to focused client.
- **Widget toolkit** (immediate-mode, software-rendered) + **AA fonts** + image
  decode for icons/wallpaper.
- Window management: decorations, resize, min/max, alt-tab, multiple workspaces.
- Native apps: terminal (port the existing one), file manager, settings.
- (North star) GPU/KMS acceleration.
- **Dependencies:** A (heap/back buffer), B (shared mem + input IPC), benefits
  from C (shared toolkit lib).

### Phase E — Network driver + TCP/IP  *(complexity: L)*
**Goal:** ping, DHCP, DNS, TCP sockets over real/virtual hardware.
- PCI: enable bus-mastering, BAR sizing, capability walk.
- **`NetDevice` trait** + registry; **DMA-ring helper**.
- **RTL8139** driver first (simplest), then **E1000/82540EM** (QEMU default).
- Integrate **`smoltcp`** in a `netd` task; **loopback** for testing.
- Socket API (syscall facade over IPC); **DHCP** + **DNS**.
- **Dependencies:** A (APIC/MSI optional but recommended; frame reclaim for
  buffers), D-independent (can proceed in parallel with GUI).
- **Unblocks:** anything online; package/app fetching; WiFi netdev integration.

### Phase F — WiFi (802.11 + WPA2)  *(complexity: XL, highest risk)*
**Goal:** associate to a WPA2 AP and route traffic through the netstack.
- **Crypto** primitives (AES-CCMP, SHA/HMAC, PBKDF2/SAE) — `no_std` crates.
- A concrete **PCIe WiFi driver** with **firmware blob loading** (from NextFS),
  DMA queues, MSI-X.
- **802.11 MLME**: scan/auth/assoc/management frames.
- **WPA2 supplicant**: EAPOL 4-way handshake, key install.
- Present WiFi as a `NetDevice` to `smoltcp`.
- **Dependencies:** A (MSI-X, RTC/entropy), E (netstack + NetDevice), firmware
  loading service, real-hardware testing.
- **Reuse option:** evaluate DDE/rump-style shims to reuse Linux/NetBSD driver
  code in a confined server (already flagged in `ROADMAP.md`), since writing a
  full WiFi driver+firmware path from scratch is multi-month.

### Phase G — Hardening & breadth  *(complexity: ongoing)*
- SMP (per-CPU run queues, locking review), UEFI boot, ACPI/power, USB, audio,
  interrupt-driven AHCI/NCQ (resolve the `PORT_IE` hang — see
  `PHASE2_INTERRUPT_ATTEMPT.md`), NextFS double/triple-indirect blocks + rename +
  symlinks + buffer cache, scheduler priorities, capability-model security review.

---

## 7. Dependency Graph (phase level)

```
        ┌─────────────────────────────────────────────┐
        │  A. Foundational debt (heap, frames, paging, │
        │     APIC, RTC)        [DO FIRST]             │
        └───────┬───────────────┬───────────────┬──────┘
                │               │               │
                ▼               ▼               ▼
   ┌────────────────┐  ┌─────────────────┐  ┌──────────────────┐
   │ B. Process &   │  │ E. NIC + TCP/IP │  │ (G. hardening,    │
   │    IPC         │  │   (smoltcp)     │  │  parallel/ongoing)│
   └───┬────────┬───┘  └────────┬────────┘  └──────────────────┘
       │        │               │
       ▼        ▼               ▼
 ┌───────────┐ ┌──────────────┐ ┌───────────────────────────┐
 │ C. Dynamic│ │ D. Display   │ │ F. WiFi (needs E + crypto │
 │   linking │ │   server/DE  │ │    + firmware loading)    │
 └─────┬─────┘ └──────┬───────┘ └───────────────────────────┘
       └──────────────┘  (C's shared toolkit lib feeds D)
```

**Critical path to "GUI-centric OS with networking":** A → B → D (desktop) and
A → E (networking), which can run in parallel; C enriches D; F is last and
largest.

---

## 8. Key Risks & Notes

- **Heap/memory ceiling is the #1 blocker.** Almost every feature here needs more
  RAM and frame reclamation. Prioritize Phase A.
- **Microkernel "honesty gap":** services/drivers/GUI currently run in-kernel.
  That's fine for velocity, but the security story (capabilities) is only real
  once they move to ring 3. Plan the relocation incrementally (shared-mem IPC is
  the enabler).
- **AHCI interrupts** remain unsolved (writes to `PORT_IE` hang). Polling works;
  revisit under Phase G with the APIC in place, as the PIC path may be implicated.
- **WiFi is a research-grade effort.** Strongly consider the driver-reuse (DDE/
  rump) route, or target a single, well-documented chip + firmware version.
- **Testing:** QEMU's default NIC is E1000 (82540EM) and it provides user-mode
  networking (SLIRP) — ideal for developing Phase E without hardware. WiFi has no
  good emulation path, so Phase F will require real hardware.

---

*End of audit. This document is intended to be the planning anchor for subsequent
implementation work; no source files were changed in producing it.*
