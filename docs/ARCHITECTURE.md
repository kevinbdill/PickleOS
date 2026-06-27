# PICKLE OS Architecture

This document describes how the PICKLE OS kernel is structured, how each subsystem
works, and the design decisions (and hard-won gotchas) behind them. It reflects
the code **as it actually exists and boots today**, not aspirations — for the
forward-looking plan see [`ROADMAP.md`](ROADMAP.md).

## 1. Philosophy

PICKLE OS is a **microkernel**. The kernel proper is responsible only for the things
that fundamentally require supervisor privilege or global arbitration:

* CPU and platform bring-up (segmentation, interrupts, timer).
* Virtual memory and physical frame allocation.
* Threads/tasks and scheduling.
* Inter-process communication (IPC).
* A capability system that mediates *all* authority.

Everything else — drivers, file systems, the network stack, the GUI — is intended
to live in user space as ordinary processes that talk to each other and to the
kernel through IPC and capabilities. Today those user-space services are future
work; the mechanisms they will rely on (IPC, capabilities, a syscall ABI, ring-3
infrastructure) already exist in the kernel.

The guiding influences are **seL4** (capability-based authority, synchronous IPC),
**Redox** (a practical Rust microkernel), and the **DDE / rump-kernel** approach to
reusing existing driver code in user space (a Phase-3 goal).

## 2. Boot flow

```
BIOS → bootloader 0.9 → long mode + page tables + phys-mem map → kernel_main(BootInfo)
```

1. The `bootloader` crate (0.9.x) provides a BIOS bootloader that switches the CPU
   to 64-bit long mode, sets up initial paging, maps all physical memory at a known
   offset (the `map_physical_memory` feature), and hands us a `BootInfo`.
2. `entry_point!(kernel_main)` (from `bootloader`) gives us a type-checked Rust entry
   point. The very first thing `kernel_main` does is a **raw serial probe** (writes
   directly to port `0x3F8`) so we get a heartbeat on the wire even before any driver
   is initialised — invaluable when debugging boot failures.
3. `init::init_all()` brings up subsystems in dependency order (see §3).
4. The boot demo spawns tasks and calls `scheduler::run()`, which never returns —
   the kernel becomes a set of cooperating tasks driven by the timer.

### The custom target (important!)

`bootloader` 0.9 copies the kernel to a fixed load address and **does not process
ELF relocations**. The stock `x86_64-unknown-none` target emits a *static-PIE*
binary; without relocation fixups it jumps into garbage and dies with no output.

`x86_64-pickleos.json` fixes this:

* `"relocation-model": "static"` and `"position-independent-executables": false`
  (and `"static-position-independent-executables": false`) → a plain, non-PIE ELF.
* `"disable-redzone": true` → mandatory in kernel code, since interrupts run on the
  same stack and would clobber the SysV red zone.
* `"features": "-mmx,-sse,+soft-float"` → no FPU/SIMD state to save in handlers.
* `target-pointer-width` / `target-c-int-width` are **numbers**, and the generated
  `metadata` key is removed — required by recent nightlies, which also need
  `json-target-spec = true` under `[unstable]` in `.cargo/config.toml`.

`build-std = ["core", "compiler_builtins", "alloc"]` rebuilds the standard library
facets for this target, with `compiler-builtins-mem` so `memcpy`/`memset` exist.

## 3. Initialisation order

`init_all()` runs, in order:

1. **Serial** — so all later steps can log.
2. **VGA** — text console cleared and ready.
3. **GDT + TSS** — segments loaded, TSS installed, IST stacks registered.
4. **IDT** — exception and interrupt handlers installed; the syscall vector wired.
5. **Memory** — `OffsetPageTable` over the bootloader's physical map; frame allocator.
6. **Heap** — heap region mapped and the global allocator initialised.
7. **PIC + PIT** — interrupt controller remapped, timer programmed, `sti`.
8. **Capabilities / IPC** — global tables initialised.

Order matters: e.g. the heap must exist before any `Box`/`Vec`, and the GDT/IDT must
be live before the timer can safely fire.

## 4. Console (`serial.rs`, `vga_buffer.rs`)

* **Serial** drives a 16550 UART on COM1 via `uart_16550`. This is the primary
  channel for headless boots and CI — the QEMU `-serial stdio` bridge shows it in
  your terminal.
* **VGA** writes the 80×25 text buffer at `0xb8000` through a `volatile` wrapper,
  with color attributes and scrolling.
* Both `_print` paths run inside `without_interrupts` so a timer interrupt can't
  preempt a half-written line and deadlock on the writer lock.

## 5. GDT & TSS (`gdt.rs`)

A flat 64-bit GDT with kernel code/data, user code/data, and the TSS descriptor.
The TSS carries:

* **IST entries** — a dedicated stack for the double-fault handler (so a stack
  overflow can still be handled) and one for the timer.
* **`rsp0`** — the stack the CPU switches to on a privilege transition (ring 3 → 0).

> **Gotcha:** the TSS is a `static mut`, not a `lazy_static`. The scheduler rewrites
> `rsp0` on every context switch (`set_kernel_stack`) so that when a user task traps
> into the kernel it lands on *its own* kernel stack. Taking `&mut` to a `lazy_static`
> for that mutation fails to compile (`E0606`), hence the raw `static mut`.

## 6. Interrupts & exceptions (`interrupts.rs`)

* A single global `IDT` installs handlers for the standard exceptions
  (breakpoint, divide-by-zero, invalid opcode, general-protection, page-fault) and
  the double-fault on its IST stack.
* The **8259 PIC** pair is remapped to vectors 32–47 (`PIC_1_OFFSET = 32`) to avoid
  colliding with CPU exception vectors.
* The **PIT** fires IRQ0 at ~100 Hz; the handler bumps the global tick counter and
  calls `scheduler::on_timer_tick()`, then sends EOI.
* The **keyboard** handler (IRQ1) reads the scancode and pushes it onto a lock-free,
  fixed-size queue (no heap allocation in the ISR) consumed by the shell task.
* The **syscall vector** `0x80` is installed with DPL=3 so user code may `int 0x80`;
  its handler address is set raw (`set_handler_addr`) to point at an assembly stub.

## 7. Memory management (`memory.rs`, `allocator.rs`)

* `memory::init` builds an `OffsetPageTable` from the bootloader's complete physical
  map. A `BootInfoFrameAllocator` hands out usable physical frames from the memory map.
* A global `MemoryManager` (behind a lock, accessed via `with_memory`) owns the page
  table and allocator so any subsystem can map regions with the right flags
  (`kernel_rw_flags`, `user_rw_flags`).
* The **heap** is a 1 MiB region at `0x_4444_4444_0000`, mapped at init and handed to a
  `linked_list_allocator::LockedHeap` registered as the `#[global_allocator]`. After
  this, `alloc` collections (`Box`, `Vec`, `String`, `BTreeMap`) work normally.

## 8. Tasks & scheduling (`task.rs`)

A `Task` holds an id, a state (`Runnable`/`Running`/`Blocked`/`Exited`), and its saved
kernel `rsp`. Tasks each own a 32 KiB kernel stack.

### Context switch

`context_switch(old_rsp: *mut u64, new_rsp: u64)` is hand-written assembly
(`global_asm!`, Intel syntax):

```
push rbp; rbx; r12..r15      ; callee-saved registers
mov [rdi], rsp               ; save old stack pointer into *old_rsp
mov rsp, rsi                 ; load the new task's stack pointer
pop r15..r12; rbx; rbp       ; restore callee-saved registers
ret                          ; return onto the new task's stack
```

This is the classic xv6/Redox "switch stacks and return" trick: because each task's
register state lives on its own stack, swapping `rsp` *is* the context switch.

### Bootstrapping a new task

A freshly spawned task has never run, so we **forge** an initial stack: the top slot
holds the address of `task_trampoline`, the entry-function pointer is smuggled into
the `r12` slot, and `rsp` is set so the first `ret` in `context_switch` "returns" into
the trampoline. `task_trampoline` enables interrupts (`sti`), `call`s the entry fn via
`r12`, and on return `call`s `task_exit_current`. The stack top is 16-byte aligned to
satisfy the SysV ABI.

### Scheduler

Round-robin over the runnable set. `on_timer_tick` (called from the IRQ with interrupts
already off) picks the next runnable task and switches to it. Invariants:

* `schedule()` assumes interrupts are disabled on entry.
* It skips the switch if the next task is the current one.
* It demotes the outgoing task `Running → Runnable`, promotes the next to `Running`,
  and sets `current = next` **before** calling `context_switch` — whoever switches *to*
  a task is responsible for marking it current.

> **Deadlock rule:** every global lock accessor (`scheduler::with`, `ipc::with_state`,
> `capability::with_tables`) wraps its critical section in `without_interrupts`. On a
> single CPU this is what prevents the preemptive timer from re-entering a lock the
> interrupted task already holds.

## 9. System calls (`syscall.rs`)

User → kernel entry is `int 0x80`. The vector points at `syscall_stub` (assembly),
which saves registers into a `SyscallFrame`, calls `syscall_dispatch`, and returns.
The ABI (arguments in registers, number in `rax`) currently exposes:

| # | Name | Meaning |
|---|------|---------|
| 1 | `SYS_PRINT`     | write a string |
| 2 | `SYS_GETPID`    | current task id |
| 3 | `SYS_YIELD`     | cooperatively yield |
| 4 | `SYS_TICKS`     | timer ticks since boot |
| 5 | `SYS_IPC_SEND`  | send a message to an endpoint |
| 6 | `SYS_IPC_RECV`  | receive a message |
| 7 | `SYS_EXIT`      | terminate the calling task |
| 8 | `SYS_CAP_CHECK` | test a capability's rights |

A `syscall3` inline-asm helper lets in-kernel code exercise the same path.

## 10. IPC (`ipc.rs`)

Synchronous, seL4-style message passing. An `Endpoint` (optionally named, with a
registry for lookup) carries fixed-size `Message`s. The primitives are `send`,
`receive`, `call` (send then block for a reply), and `reply`. Senders block until a
receiver is ready and vice-versa, so no buffering or message loss. The boot demo's
ping/pong tasks use `call`/`reply` to bounce a counter back and forth, which is the
visible proof IPC + blocking + scheduling all cooperate.

## 11. Capabilities (`capability.rs`)

Authority is represented by **capabilities**: unforgeable references that name a kernel
`Object` (endpoint, task, memory region, …) together with a set of `Rights` (a manual
bitset — `READ`, `WRITE`, `SEND`, `GRANT`, etc.). Each task has a `CapTable`. Operations
are `mint`, `lookup`, `check`, `grant` (copy a cap to another table, possibly with
reduced rights) and `revoke`. This is the substrate the future user-space services use
to delegate least-privilege authority instead of relying on ambient permissions.

## 12. Shell (`shell.rs`)

An in-kernel task that consumes the keyboard scancode queue, decodes it with
`pc-keyboard`, and offers a small command set: `help`, `ps`, `pid`, `ticks`, `uptime`,
`echo`, `ipc`, `caps`, `mem`, `int3` (deliberately fires a breakpoint to demo exception
handling) and `clear`. It is primarily a live, interactive way to poke every subsystem
above. The scancode queue is a fixed array (no heap) so the keyboard ISR never allocates.

## 13. Known limitations (today)

* **No user-space process yet.** Ring-3 *infrastructure* exists (user segments,
  syscall DPL=3, per-task `rsp0`), but nothing loads and jumps into a separate user
  binary — that needs an ELF loader and address-space-per-process (Phase 1).
* **Single CPU.** No SMP/APIC; the PIC+PIT path is single-core.
* **BIOS only.** UEFI is a later target.
* **Round-robin scheduling** with no priorities or accounting beyond ticks.

These are deliberate Phase-0 boundaries, not bugs — see the roadmap.
