# PICKLE OS 🥒

A from-scratch **microkernel operating system written in Rust** for x86_64. Boots to a graphical desktop with window manager, networking, filesystem, IPC, and an interactive shell.

[![Boot Screenshot](pickleos_boot_screenshot.png)](https://pickleos.org)

---

## What works today (verified booting in QEMU)

| Subsystem | Status |
|-----------|--------|
| **Boot to graphical desktop** (BIOS/VESA via `bootloader` 0.11) | ✅ Working |
| **Compositing window manager** — desktop, taskbar, draggable windows | ✅ Working |
| **Window server** — shared buffers, event queues, window registry | ✅ Working |
| **Round-robin cooperative + timer scheduler** | ✅ Working |
| **Paging / per-task address spaces** | ✅ Working |
| **Ring-3 user processes** (ELF loader, `fork`/`exec`/`wait`/`exit`) | ✅ Working |
| **Pipes + signals** (`kill`/`signal`/`sigreturn`) | ✅ Working |
| **Synchronous IPC + capabilities** | ✅ Working |
| **Custom on-disk FS (NextFS)** | ✅ Working (fragile) |
| **AHCI/SATA block driver** | ✅ Working (polled) |
| **PS/2 keyboard + mouse** | ✅ Working |
| **TCP/IP via smoltcp + e1000 NIC** | ✅ Partial |
| **In-kernel shell** w/ pipes & redirect | ✅ Working |
| **46 syscalls** | ✅ Working surface |
| **4 user-space GUI apps** (launcher, file manager, text editor, taskbar) | ✅ Working |

**Sample boot output:**
```
PICKLE OS: booting microkernel v0.1.0
[wm] window-server core online (max 16 windows)
[pci] enumeration complete: 7 device(s) found
[ahci] found controller — 8086:2922
[e1000] hardware initialized, link_up=true
net :: stack configured with IP 10.0.2.15/24
[heartbeat] tick 5
[wm-selftest] => PASS
[net-demo] TCP echo server listening on port 7
PICKLE OS shell — type 'help'
pickleos>
```

---

## Repository layout

```
pickleos/
├── Cargo.toml                  # workspace (kernel + userspace)
├── rust-toolchain.toml         # pinned nightly
├── x86_64-pickleos.json        # custom bare-metal target
├── .cargo/config.toml          # build-std config
├── Makefile                    # build / run / debug / test targets
├── scripts/
│   ├── run-qemu.sh             # build + boot
│   └── debug-qemu.sh           # GDB debugging
├── docs/
│   ├── ARCHITECTURE.md         # design docs
│   ├── PRODUCTION_ROADMAP.md   # production gap analysis
│   └── ROADMAP.md              # phased plan
├── kernel/src/
│   ├── main.rs                  # entry point + init
│   ├── task.rs                  # scheduler + context switch
│   ├── memory.rs                # paging + frame allocator
│   ├── syscall.rs              # syscall dispatch
│   ├── ipc.rs                  # synchronous IPC
│   ├── capability.rs           # capability tables
│   ├── gui.rs                  # compositing window manager
│   ├── wm.rs                   # window server core
│   ├── interrupts.rs           # IDT, PIC, exceptions
│   ├── shell.rs                # in-kernel shell
│   ├── fs/                     # NextFS filesystem
│   ├── driver/                 # AHCI, e1000, keyboard, mouse, PCI, DMA
│   ├── net/                    # TCP/IP (smoltcp)
│   └── signal.rs               # POSIX signals
├── userspace/                  # user-space programs
│   ├── libpickleos/            # user-space library
│   ├── filemanager/            # GUI file manager app
│   ├── texteditor/             # GUI text editor app
│   ├── launcher/               # GUI launcher app
│   └── taskbar/                # GUI taskbar app
└── disk0.img, disk1.img        # scratch SATA disk images
```

---

## Building & running

### Prerequisites

```bash
# Rust nightly + components
rustup component add rust-src llvm-tools-preview

# Boot image tool
cargo install bootloader_linker --version 0.1.7

# QEMU
sudo apt-get install qemu-system-x86      # Debian/Ubuntu
```

### Build & boot

```bash
make build      # compile kernel + userspace (release)
make image      # produce bootable bios.img
make run        # headless: serial console in your terminal
make run-display # graphical: QEMU window with desktop GUI
```

---

## Production roadmap

PICKLE OS is a genuine hobby/teaching microkernel — it boots, runs, displays a GUI, and networks. But it is **not production-ready**. The gap is measured in engineer-years.

See [`docs/PRODUCTION_ROADMAP.md`](docs/PRODUCTION_ROADMAP.md) for the brutal truth and phased plan.

**Phase 1 — Critical Stability (current focus):**
1. Fix NextFS concurrency race (per-inode locking)
2. Fix window Z-order / topmost bug
3. Remove hard-coded 320×240 window ceiling
4. Build test harness + CI
5. Kernel panic → serial backtrace + 24h soak test
6. Lock ordering audit

---

## Why?

Because building an OS from scratch is one of the most satisfying things you can do with a computer. PICKLE OS is a real, working kernel with a real GUI that runs on real hardware — and it's only getting better.

## License

MIT OR Apache-2.0
