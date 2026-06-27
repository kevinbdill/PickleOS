# PickleOS — Production Roadmap & Brutally Honest Gap Analysis

> **Scope:** This document audits the *current* PickleOS codebase (~16,800 lines of
> kernel Rust + a small C/Rust userspace) and lays out, without sugar-coating,
> everything that stands between it and a *production-ready* operating system that
> can boot and run reliably on real hardware.
>
> **Date:** June 2026 · **Target:** x86_64 · **Boot:** BIOS/VESA (bootloader 0.11)
>
> **TL;DR:** PickleOS is an impressive *hobby/teaching microkernel* with a real
> GUI, IPC, capabilities, a TCP/IP stack, and process management. It is **not**
> close to production. The honest gap is measured in **multiple engineer-years**.
> The single biggest lies a hobby OS tells itself are "it boots in QEMU so it's
> almost done" and "the features exist so they work." Most subsystems here are
> *demonstrations*, not *dependable services*. This roadmap separates the two.

---

## 1. Critical Analysis — Where We Actually Stand

### 1.1 What genuinely exists and works

These are real, not vaporware. Credit where due:

| Subsystem | State | Evidence in tree |
|---|---|---|
| Boot to graphical desktop | Working (QEMU) | `main.rs`, `framebuffer.rs`, `gui.rs` |
| Round-robin cooperative+timer scheduler | Working | `task.rs` (`Scheduler::schedule`) |
| Paging / per-task address spaces | Working | `memory.rs`, `create_user_mapper` |
| Ring-3 user processes (ELF loader) | Working | `elf.rs`, `task.rs::do_exec` |
| `fork` / `exec` / `wait` / `exit` | Working | `task.rs`, `args_test`, `fork_test` |
| Pipes + signals (kill/signal/sigreturn) | Working | `vfs.rs`, `signal.rs`, `signal_test` |
| Synchronous IPC + capabilities | Working | `ipc.rs`, `capability.rs` |
| Custom on-disk FS (NextFS) | Working (fragile) | `fs/nextfs.rs` |
| AHCI/SATA block driver | Working (polled) | `driver/ahci.rs` |
| PS/2 keyboard + mouse | Working | `driver/keyboard.rs`, `mouse.rs` |
| TCP/IP via smoltcp + e1000 NIC | Partial | `net/stack.rs`, `driver/e1000.rs` |
| Window manager + client GUI apps | Partial | `wm.rs`, `gui.rs`, 4 apps |
| In-kernel shell w/ pipes & redirect | Working | `shell.rs` |
| 46 syscalls | Working surface | `syscall.rs` |

### 1.2 The uncomfortable truths

1. **It only boots under QEMU/BIOS.** There is **no UEFI** path, **no ACPI**
   parsing, **no APIC**, and the system uses the **legacy 8259 PIC**. Real
   post-2020 hardware frequently ships with no 8259, no PS/2 controller, and
   UEFI-only firmware. Today, PickleOS would very likely **not boot on a real
   laptop at all**.

2. **It is single-core.** No SMP, no AP startup, no per-CPU state, no APIC
   timer. Every modern machine has 4–16+ cores; using one is a non-starter for
   "production."

3. **"Microkernel" is aspirational.** Drivers (AHCI, keyboard, mouse, e1000,
   NextFS) all run **in-kernel at Ring 0**. The IPC/capability machinery exists,
   but the security boundary it's meant to enforce is not yet used for the things
   that matter. A driver bug = kernel crash.

4. **No security model is actually enforced.** `uid`/`gid`/permission bits exist
   in NextFS and the VFS *as data*, but there is no login, no authentication, no
   privilege separation, and no real check that stops process A from touching
   process B's files. Capabilities guard MMIO, but `KernelIoGuard` bypasses them
   wholesale during exec.

5. **Concurrency correctness is shaky.** The known **filesystem race** (NextFS
   `NotFound` under concurrent `exec`) and the **window Z-order bug** are
   symptoms of a deeper issue: shared state guarded by coarse spinlocks with
   ad-hoc ordering, written for the single-threaded happy path.

6. **Reliability is unproven.** There is no test harness beyond boot-time
   self-tests and serial assertions. No fuzzing, no stress testing, no
   long-running soak tests, no fault injection. "It worked in the screenshot" is
   the current bar.

7. **Persistence is risky.** NextFS has no journaling, no `fsync` durability
   guarantees, and no crash-consistency story. A power cut mid-write can corrupt
   the filesystem.

### 1.3 Known active bugs (carry-over)

| Bug | Impact | Root cause (suspected) |
|---|---|---|
| Window Z-order / topmost on boot | Launcher/Taskbar not visible | Compositor stacking + focus order in `gui.rs` |
| NextFS `NotFound` under exec bursts | Intermittent launch failure | Non-reentrant single-lock lookup in `nextfs.rs` |
| Compositor rejected windows >320×240 | Worked around, not fixed | Hard-coded `MAX_W`/`MAX_H` in `wm.rs` |
| MMIO caps bypassed via `KernelIoGuard` | Security hole | Loader needs disk I/O on kernel authority |

---

## 2. Complete Gap Analysis (by category)

Legend for effort: **S** = Small (days), **M** = Medium (1–3 wks), **L** = Large
(1–3 mo), **XL** = Very Large (3+ mo / multi-person). Estimates assume one
experienced kernel engineer.

### 2.1 Kernel Core

| Component | Status | Gap | Effort |
|---|---|---|---|
| Scheduler | Round-robin, single queue | No priorities, no fairness (CFS/MLFQ), no nice/affinity, no real-time class | **L** |
| SMP / multicore | **Absent** | AP boot (INIT-SIPI-SIPI), per-CPU GDT/TSS, per-CPU runqueues, IPIs | **XL** |
| APIC / x2APIC | **Absent** (8259 PIC) | Local APIC, IO-APIC, APIC timer, MSI/MSI-X | **L** |
| Synchronization | Spinlocks only | Need sleeping mutexes, RW locks, RCU-like, lock ordering discipline, deadlock audit | **L** |
| Memory mgmt | Paging + bump mmap | No demand paging, no swap, no COW (fork deep-copies!), no `mprotect`, no huge pages, no OOM handling | **L** |
| Kernel heap | linked-list allocator | No slab/slub, fragmentation under load, no per-CPU caches | **M** |
| Time | Timer ticks only | No wall-clock (RTC/CMOS), no monotonic clock, no high-res timers, no `clock_gettime` | **M** |
| Syscall ABI | `int 0x80`, 46 calls | Missing dozens (see 2.x); no `syscall`/`sysret` fast path; no seccomp/filtering | **L** |
| Preemption safety | Coarse | Kernel preemption hazards, no formal interrupt-disable discipline audit | **M** |
| Userspace threads | **Absent** (process only) | No `clone(CLONE_THREAD)`, no TLS, no futex | **L** |

**Missing syscalls (representative, not exhaustive):** `dup`/`dup2`,
`fcntl`, `poll`/`select`/`epoll`, `mprotect`, `getcwd`/`chdir`, `getuid`/`setuid`/
`getgid`/`setgid`, `getdents`, `rename`, `link`/`symlink`/`readlink`, `fstat`/
`lstat`, `ioctl`, `clock_gettime`/`nanosleep`, `futex`, `clone`, `setpgid`/
`getpgid`/`setsid` (job control), `umask`, `access`, `sync`/`fsync`,
`sigaction`/`sigprocmask` (only basic `signal` exists), `wait4`/`waitpid` options.

### 2.2 Drivers

| Hardware | Status | Gap | Effort |
|---|---|---|---|
| Storage — AHCI/SATA | Polled, basic R/W | No IRQ-driven completion, no NCQ, no error recovery/timeouts, no hotplug | **M** |
| Storage — NVMe | **Absent** | Required for modern SSDs; full driver | **L** |
| Storage — USB mass storage | **Absent** | Needs USB stack first | **XL** |
| USB stack | **Absent** | xHCI host controller, USB core, HID, hub, mass-storage class | **XL** |
| Input — PS/2 kbd/mouse | Working | Legacy only; no USB HID; no key remap/layouts beyond basic | **M** |
| Display — VESA framebuffer | Working | No GPU acceleration, no mode setting (fixed res), no DRM/KMS, no multi-monitor | **XL** |
| Display — GPU (Intel/AMD/virtio-gpu) | **Absent** | At minimum virtio-gpu for VMs; real GPU is XL+ | **L–XL** |
| Network — e1000 | Partial | Single NIC model; no virtio-net, no Realtek, no Wi-Fi, no offload | **L** |
| Audio | **Absent** | HDA/AC'97/virtio-sound + mixing | **L** |
| RTC / CMOS clock | **Absent** | Wall-clock time source | **S** |
| PCI | Enumeration only | No bridge recursion, no PCIe ECAM, no resource allocation, no MSI | **M** |
| ACPI | **Absent** | Table parsing (MADT/FADT), power-off, reboot, thermal, lid/battery | **L** |
| Power management | **Absent** | No suspend/resume (S3), no CPU freq scaling, no idle C-states | **L** |
| Serial / UART | Working (debug) | Fine for console; no general TTY layer | **S** |

### 2.3 Networking

| Layer | Status | Gap | Effort |
|---|---|---|---|
| L2 driver (e1000) | Partial | IRQ-driven RX/TX maturity, multi-buffer rings, stats, error handling | **M** |
| Stack (smoltcp) | Wired, polled | No background poll thread w/ proper timers, no socket blocking semantics fully tied to scheduler | **M** |
| IP | IPv4 (+IPv6 feature on) | IPv6 untested; no fragmentation/reassembly verified | **M** |
| TCP | smoltcp present | Congestion control is smoltcp's; needs real-world testing, retransmit tuning | **M** |
| UDP | Present | OK for basics | **S** |
| ICMP | Feature enabled | Ping responder/initiator wiring | **S** |
| DHCP client | **Absent** (not wired) | Required for real networks (auto IP) | **M** |
| DNS resolver | smoltcp feature, not wired | Name resolution end-to-end | **M** |
| Socket API (BSD) | Syscalls 39–46 exist | Need `getsockopt`/`setsockopt`, non-blocking, `poll` integration, error mapping to errno | **M** |
| TLS | **Absent** | rustls/embedded-tls port (no_std), entropy source | **L** |
| Higher protocols | **Absent** | HTTP client, NTP, etc. (userspace) | **M** |

### 2.4 Filesystem

| Aspect | Status | Gap | Effort |
|---|---|---|---|
| NextFS core | Working | Single-indirect only → small max file size; no double/triple indirect or extents | **M** |
| Concurrency | **Buggy** | The `NotFound` race; needs per-inode locking, lookup that's reentrant under concurrency | **M** |
| Crash consistency | **Absent** | No journaling/log, no atomic metadata updates, no `fsync` durability | **L** |
| Permissions enforcement | Data only | uid/gid/mode stored but not *enforced* on open/exec; no ACLs | **M** |
| VFS | Basic | No mount namespaces, limited mount table, no `/proc`, `/sys`, `/dev` as real FSes | **L** |
| FAT32 | **Absent** | Needed for UEFI ESP + interop with USB drives | **M** |
| ext4 (read at least) | **Absent** | Interop with Linux-formatted disks | **L** |
| Page/buffer cache | **Absent/minimal** | No unified cache, no writeback, no read-ahead → slow + unsafe | **L** |
| tmpfs / ramfs | Partial (`memfs`) | Generalize as a real VFS-mounted fs | **S** |
| Device nodes | **Absent** | `/dev/null`, `/dev/zero`, tty, block devs as files | **M** |

### 2.5 Security

| Control | Status | Gap | Effort |
|---|---|---|---|
| Process isolation (memory) | Working (paging) | Solid baseline | — |
| Privilege separation | Weak | Drivers in Ring 0; `KernelIoGuard` bypass; trusted kernel surface is huge | **XL** |
| Users / authentication | **Absent** | No login, no `/etc/passwd`, no PAM-equivalent, no sessions | **L** |
| Permission enforcement | **Absent** | DAC checks on every syscall path (open/exec/kill/signal) | **M** |
| Capabilities (real use) | Partial | Used for MMIO; extend to all kernel objects; revocation audit | **L** |
| Mandatory access control | **Absent** | SELinux/AppArmor-style — likely out of scope near-term | **XL** |
| ASLR / KASLR | **Absent** | Randomize user + kernel layout | **M** |
| Stack protections | Unknown/Absent | Stack canaries, guard pages (some), W^X enforcement audit | **M** |
| Secure boot chain | **Absent** | UEFI Secure Boot, signed kernel, measured boot/TPM | **L** |
| Disk encryption | **Absent** | LUKS-equivalent, key management | **L** |
| Entropy / RNG | **Absent** | `getrandom`, CSPRNG seeded from hw (RDRAND/jitter) | **M** |
| Syscall hardening | **Absent** | Argument validation audit, seccomp-style filtering | **M** |

### 2.6 System Services

| Service | Status | Gap | Effort |
|---|---|---|---|
| Init system | Minimal (`inittab`, reaper) | No dependency ordering, no service supervision/restart, no targets/runlevels, no socket activation | **L** |
| Logging | serial `println!` only | No structured logs, no `/var/log`, no `dmesg` ring buffer surfaced to userspace, no log rotation | **M** |
| Device manager | **Absent** | No udev-equivalent, no `/dev` population, no hotplug events | **L** |
| Service registry/IPC bus | `registry` service exists | No system bus (D-Bus-like), no service discovery for apps | **M** |
| Time sync | **Absent** | NTP client + RTC set | **M** |
| Power management UX | **Absent** | Clean shutdown/reboot path (needs ACPI), battery UI | **M** |
| Crash reporting | **Absent** | Kernel oops capture to disk, core dumps | **M** |
| Config management | **Absent** | `/etc` conventions, parsers | **S** |

### 2.7 User Experience

| Item | Status | Gap | Effort |
|---|---|---|---|
| Shell | In-kernel, rich builtins | Not a *userspace* shell; no scripting (loops/vars/conditionals), no job control, no globbing/pipes-from-userspace, no `$PATH` exec | **L** |
| Core utilities | A few (in-kernel) | Real userspace coreutils: `ls cp mv rm cat mkdir chmod ps kill df du grep find sort head tail less ln stat date` etc. | **L** |
| C library | Custom `libpickleos` (partial) | No full libc (musl/newlib port) → can't run existing software | **XL** |
| Terminal emulator (GUI) | Partial | Real VT100/ANSI terminal app over pty | **M** |
| Window manager UX | Partial + buggy | Z-order fix, resize, minimize/maximize, alt-tab, decorations, clipboard | **L** |
| Package manager | **Absent** | Format, repo, dependency resolution, install/upgrade/remove | **L** |
| Fonts / i18n | 8x8 bitmap only | TrueType rendering, Unicode, input methods | **L** |
| Settings / control panel | **Absent** | Display, network, users, time config UI | **M** |
| Default apps | 4 demo apps | File mgr/editor are size-constrained demos; need real file dialogs, etc. | **M** |

### 2.8 Developer Tools

| Tool | Status | Gap | Effort |
|---|---|---|---|
| On-device toolchain | **Absent** | Compiler (cc/rustc), assembler, linker, make — enormous (libc + toolchain port) | **XL** |
| Debugger | **Absent** | `ptrace`-equivalent, gdbstub, symbol support | **L** |
| Profiler / perf | **Absent** | Sampling profiler, counters | **M** |
| Crash diagnostics | Basic (fault regs to serial) | Symbolized backtraces, kernel debugger (kdb), QEMU+gdb workflow docs | **M** |
| Test framework | Boot self-tests only | Unit tests (host), integration harness, CI, fuzzing, KASAN-style sanitizer | **L** |
| Tracing | **Absent** | ftrace/eBPF-lite, event tracing | **L** |

### 2.9 Hardware Support / Boot

| Item | Status | Gap | Effort |
|---|---|---|---|
| BIOS boot (QEMU) | Working | Legacy only | — |
| UEFI boot | **Absent** | UEFI loader, GOP framebuffer, ESP/FAT32, memory map handoff | **L** |
| ACPI | **Absent** | MADT (CPUs/IRQs), FADT (power), DSDT (devices) | **L** |
| Real-hardware bring-up | **Untested** | Almost certainly fails: no APIC, no USB input, fixed framebuffer assumptions, no NVMe | **XL** |
| Multiple framebuffer modes | Fixed 1280×720 | Mode negotiation, EDID, scaling | **M** |
| 32-bit / other arch | x86_64 only | (Likely out of scope) ARM64 port | **XL** |
| Firmware quirks | None handled | Real machines need quirk tables, fallbacks | **L** |

---

## 3. Implementation Roadmap (Phased)

This is ordered by **dependency and risk**, not by glamour. Each phase has an
explicit **exit criterion** — a measurable "done."

### Phase 1 — Critical Stability (make what exists *dependable*)
**Goal: nothing that already "works" should fail intermittently.**

1. Fix the **NextFS concurrency race** — per-inode locking, reentrant lookup,
   stress test with concurrent `exec` (1000 iterations clean). **[M]**
2. Fix the **window Z-order/topmost** bug; define deterministic stacking +
   focus-raises-window. **[M]**
3. Remove hard-coded `320×240`/`MAX_W`/`MAX_H` window ceiling properly. **[S]**
4. Build a **real test harness**: host-side unit tests where possible, a
   QEMU-driven integration runner with pass/fail exit codes, wire into CI. **[M]**
5. Add **kernel panic → serial backtrace with symbols** and a soak test
   (24h boot loop + app churn) that must survive. **[M]**
6. Audit **lock ordering**; document and enforce; remove the worst spinlock
   hold-across-I/O hazards. **[M]**

**Exit:** 24-hour soak with app launch/close churn and FS I/O, zero crashes,
zero FS corruption; CI green.

### Phase 2 — Essential Syscalls & Driver Hardening
**Goal: enough OS surface to host real programs and survive driver errors.**

1. Syscall surface: `dup/dup2`, `fcntl`, `poll`/`select`, `getcwd`/`chdir`,
   `getdents`, `rename`, `fstat`/`lstat`, `mprotect`, `nanosleep`,
   `clock_gettime`, `ioctl`, `access`, `fsync`. **[L]**
2. **RTC/CMOS** driver + monotonic + wall clock. **[S]**
3. **AHCI**: IRQ-driven completion, timeouts, error recovery. **[M]**
4. **COW fork** + demand paging + `mprotect` (perf + correctness). **[L]**
5. **Sleeping mutex / RW lock / futex** primitives; userspace threads via
   `clone`. **[L]**
6. Device nodes + a minimal `/dev`, `/proc` (at least `ps`, `meminfo`). **[M]**

**Exit:** A non-trivial ported userspace program (e.g., a real shell + coreutils
subset) runs from disk via `$PATH`, with working pipes, redirection, and job
control basics.

### Phase 3 — Networking & Filesystem Maturity
**Goal: talk to a real network; don't lose data on power loss.**

1. **DHCP client** + **DNS** wired end-to-end; background net poll task with
   proper smoltcp timers tied to the scheduler. **[M]**
2. Blocking/non-blocking sockets fully integrated; `setsockopt`/`getsockopt`;
   errno mapping. **[M]**
3. **FAT32** (R/W) — needed for UEFI ESP and USB interop. **[M]**
4. **NextFS journaling** (or replace with a journaled design) for crash
   consistency; real `fsync`. **[L]**
5. **Unified buffer/page cache** with writeback + read-ahead. **[L]**
6. **TLS** (no_std rustls/embedded-tls) + CSPRNG/entropy. **[L]**
7. ext4 read-only support (Linux interop). **[L]**

**Exit:** Fetch an HTTPS URL over DHCP-assigned IP with DNS; pull power mid-write
1000× with no FS corruption.

### Phase 4 — Security & Multi-User
**Goal: a real trust model.**

1. **Enforce DAC**: uid/gid/mode checks on open/exec/kill/signal; `setuid`
   family; `umask`. **[M]**
2. **Login + auth**: `/etc/passwd`+`shadow` (hashed), getty/login, sessions. **[L]**
3. **Move drivers to Ring 3** (true microkernel): AHCI, keyboard, e1000 as
   userspace servers over IPC + capabilities; eliminate `KernelIoGuard`
   blanket bypass. **[XL]**
4. **KASLR/ASLR**, guard pages, W^X enforcement audit, stack canaries. **[M]**
5. **Syscall argument hardening** + seccomp-style filter. **[M]**
6. **Entropy/RNG** subsystem (`getrandom`). **[M]**

**Exit:** Two unprivileged users cannot read each other's files or signal each
other's processes; a driver crash (Ring 3) does not panic the kernel.

### Phase 5 — User Experience & Applications
**Goal: someone could actually *use* it.**

1. **Userspace shell** with scripting (vars, control flow, globbing, job
   control). **[L]**
2. **Coreutils** suite in userspace. **[L]**
3. **libc port** (musl or newlib) — unlocks the existing software ecosystem. **[XL]**
4. **Window manager UX**: resize/min/max, alt-tab, decorations, **clipboard**,
   real terminal emulator over pty. **[L]**
5. **TrueType + Unicode** text rendering; input methods. **[L]**
6. **Package manager** (format + repo + deps + install/remove). **[L]**
7. **Settings app** (display/network/users/time). **[M]**

**Exit:** Install a package from a repo, run it from a GUI launcher, copy/paste
between two apps, edit and save a UTF-8 document.

### Phase 6 — Production Hardening & Real Hardware
**Goal: boots and survives on actual machines.**

1. **UEFI boot** (loader, GOP, ESP) — and keep BIOS fallback. **[L]**
2. **ACPI** parsing: MADT (CPUs/IRQs), FADT (power-off/reboot), thermal. **[L]**
3. **APIC/x2APIC + IO-APIC + MSI/MSI-X**; retire the 8259. **[L]**
4. **SMP**: AP startup, per-CPU runqueues, IPIs, scalable locks. **[XL]**
5. **USB stack** (xHCI + HID + mass storage) — modern input + storage. **[XL]**
6. **NVMe** driver. **[L]**
7. **Power management**: S3 suspend, CPU freq, clean shutdown via ACPI. **[L]**
8. **GPU**: virtio-gpu (VMs) and a path toward a real KMS driver. **[L–XL]**
9. **Secure boot** chain + (optional) disk encryption + TPM measured boot. **[L]**
10. **Hardware bring-up matrix**: test on ≥3 real machines; quirk handling. **[XL]**
11. **Audio** (HDA/virtio-sound). **[L]**
12. Multi-resolution/EDID/multi-monitor display. **[M]**

**Exit:** Cold-boots from a USB stick on at least one physical UEFI laptop to the
GUI, with working USB keyboard/mouse, networking, storage, and clean shutdown.

---

## 4. Effort Summary

### 4.1 By phase (rough, single experienced kernel engineer)

| Phase | Theme | Rough calendar effort |
|---|---|---|
| 1 | Critical stability | 1.5–2.5 months |
| 2 | Syscalls + driver hardening | 3–5 months |
| 3 | Networking + FS maturity | 4–7 months |
| 4 | Security + multi-user | 5–9 months (Ring-3 drivers dominate) |
| 5 | UX + apps | 6–12 months (libc dominates) |
| 6 | Production HW (UEFI/ACPI/SMP/USB) | 9–18 months |

**Total to "production-ready on real hardware": ~2.5–4.5 engineer-years** for a
single strong engineer; **~12–24 months** with a focused team of 3–4. The long
poles are **SMP, USB, libc, UEFI/ACPI/APIC, and Ring-3 driver migration** — each
is independently a major project.

### 4.2 The "very large" items that dominate the schedule

- **libc port (musl/newlib)** — gates the entire software ecosystem. **XL**
- **USB stack (xHCI + classes)** — gates input/storage on modern HW. **XL**
- **SMP / multicore** — gates performance credibility. **XL**
- **Ring-3 driver migration** — gates the "microkernel" security claim. **XL**
- **Real-hardware bring-up** — gates the word "production." **XL**
- **GPU/KMS (real)** — gates a modern desktop. **XL**

### 4.3 Highest-leverage next 90 days (recommendation)

If the goal is *credibility and momentum*, do **Phase 1 in full** plus the RTC
clock and the DHCP/DNS wiring from Phase 3. Rationale:

1. Fixing the FS race and Z-order bug removes the two most visible "it's flaky"
   signals.
2. A real **test harness + soak test** converts "works in a screenshot" into
   "provably stable," which every subsequent phase depends on.
3. DHCP+DNS+RTC are small, high-visibility wins ("it gets on the network and
   knows the time").

Avoid the temptation to start USB or SMP first — they are long, deep, and will
stall visible progress for months while the existing flakiness remains.

---

## 5. Honest Bottom Line

PickleOS today is a **genuinely impressive teaching/hobby microkernel**: it has a
real GUI, real processes, real IPC, a real (if fragile) filesystem, and a real
TCP/IP stack. That is far beyond most hobby OSes.

It is **not** production-ready, and the distance is large but well-understood:

- It would **likely not boot on a modern physical machine** (no UEFI/ACPI/APIC,
  legacy PS/2 input, no USB, fixed framebuffer).
- It is **single-core** and uses **coarse spinlocks** — neither performant nor
  obviously correct under contention.
- Its **security model is declared but not enforced**, and its **drivers run in
  the kernel**, contradicting the microkernel safety premise.
- It has **no durability guarantees**, **no test/soak infrastructure**, and **no
  libc**, so it can't yet host the existing software world.

None of this is a criticism of the work done — it's the standard, unavoidable gap
between "boots in QEMU and demos features" and "a dependable OS people run on real
machines." The roadmap above is the realistic path. Start with stability
(Phase 1); resist starting the XL items until the foundation is provably solid.
