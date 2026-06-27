# Phase 2 Implementation Status

## Summary

Phase 2 brings up the **core OS services** of PICKLE OS — Init, Registry, VFS, and a
MemFS backend — as independent tasks that communicate **exclusively over IPC**,
plus an interactive shell with filesystem commands. The end-to-end path
(`shell → VFS → MemFS` over IPC, with service discovery through the Registry)
is implemented and verified on a headless boot.

Phase 2 is **functionally complete** for its exit criterion: the system boots,
brings up the service stack, and can create, list, read, stat, and remove files
through the VFS service over IPC.

## Architecture note: where the services run

The Phase 2 services are implemented as **in-kernel tasks** that interact only
through the kernel's IPC `call`/`reply` primitives and named endpoints — exactly
the API a user-space process uses. This is a deliberate, standard microkernel
bring-up step:

- The **component boundaries and protocols are real** (each service owns an
  endpoint; clients never touch another service's internals — they send
  messages).
- The single place that assumes a shared address space is the request/response
  *marshalling* (boxed pointers passed through the inline message words). When
  the services migrate to isolated user address spaces (pending the page-table
  deep-copy work tracked below), only that marshalling layer changes to
  shared-memory or by-value copying — the protocols and clients stay identical.

This keeps the design honest while side-stepping the known page-table-sharing
limitation for separate user address spaces (see "Known limitations").

## Completed ✅

### 1. SYS_PRINT visibility fix
`SYS_PRINT` now mirrors output to **both** the VGA console and the serial port.
Previously, user-task output went only to VGA and was invisible on a headless
(serial-only) boot. User tasks (`hello`) now print correctly:
`[user] Syscalls work!` is observed over serial.

### 2. Extended Syscall Interface
- **SYS_SLEEP (9)**, **SYS_SPAWN (10)** (stub), **SYS_MMAP (11)**, **SYS_MUNMAP (12)**
- `sys_mmap_impl()` / `sys_munmap_impl()` anonymous user mappings
- `sleep_ticks()` in the task module

### 3. User Library — `libpickleos`
`no_std` Rust library at `userspace/libpickleos/` with `syscall`, `ipc`,
`capability`, `process`, and `memory` modules, a panic/alloc-error handler, and
`print!`/`println!` macros.

### 4. MemFS backend (`kernel/src/services/memfs.rs`)
In-memory filesystem: a path → node map (file bytes or directory). Operations:
`create`, `mkdir`, `write`, `read`, `readdir`, `stat`, `remove`. Seeds a root
`/`, `/etc`, `/welcome.txt`, and `/etc/motd` at startup. Path normalization and
parent-existence checks are enforced; `remove` refuses non-empty directories.

### 5. VFS server (`kernel/src/services/vfs.rs`)
A service task owning the `"vfs"` endpoint. Implements the request/response
protocol (`Create`, `Mkdir`, `Write`, `Read`, `ReadDir`, `Stat`, `Remove`) over
IPC and dispatches to MemFS. Ships client convenience wrappers
(`vfs::read`, `vfs::write`, `vfs::readdir`, `vfs::stat`, `vfs::create`,
`vfs::mkdir`, `vfs::remove`) used by the shell and the self-test.

### 6. Registry server (`kernel/src/services/registry.rs`)
A service task owning the `"registry"` endpoint. Maintains a name → endpoint
table with `Register` / `Lookup` / `List` over IPC, plus client helpers
(`registry::register`, `registry::lookup`, `registry::list`).

### 7. Init server (`kernel/src/services/init.rs`)
First service task. Brings up Registry then VFS in dependency order, waits for
each endpoint to publish, registers both in the Registry, prints the service
inventory, and lives on as the parent/reaper. Also launches the boot self-test.

### 8. Shell filesystem commands (`kernel/src/shell.rs`)
New commands, all routed through the VFS over IPC:
`services`, `ls [path]`, `cat <path>`, `write <path> <text>`, `touch <path>`,
`mkdir <path>`, `rm <path>`, `stat <path>`.

### 9. Boot self-test (`services::vfs_selftest_task`)
Because the interactive shell needs a PS/2 keyboard (not available on a headless
serial boot), a one-shot self-test runs at boot as a pure VFS *client* and logs
results to serial. Verified output:

```
[init] all core services online; registry knows 2 service(s):
[init]   - registry
[init]   - vfs
[selftest] === VFS over IPC self-test ===
[selftest] registry resolved 'vfs' -> endpoint 3
[selftest] ls / -> ["etc", "welcome.txt"]
[selftest] cat /welcome.txt -> "Welcome to PICKLE OS Phase 2!\n..."
[selftest] write /tmp/hello.txt -> 29 bytes
[selftest] read back /tmp/hello.txt -> "hello from the VFS self-test\n"
[selftest] stat /tmp/hello.txt -> size=29 is_dir=false
[selftest] ls /tmp -> ["hello.txt"]
[selftest] rm /tmp/hello.txt -> ok
[selftest] confirmed removed (NotFound)
[selftest] === self-test complete ===
```

## Testing

- **Build**: `cargo build` — clean (warnings only, pre-existing).
- **Headless boot**: `scripts/test-boot.sh [seconds]` builds the bootimage,
  boots QEMU with serial→stdout, and prints captured output. The service
  bring-up and VFS self-test pass end-to-end (see output above).
- **Interactive**: `scripts/run-qemu.sh --display` opens a VGA window; the shell
  `ls`/`cat`/`write`/`mkdir`/`rm`/`stat`/`services` commands operate against the
  live VFS service.

## Known limitations / future work

1. **Separate user address spaces.** L4 entry cloning currently shares L3 page
   tables between user processes (documented in Phase 1 notes). Migrating the
   Phase 2 services from in-kernel tasks into isolated ring-3 processes requires
   deep-copying page tables (or per-process address spaces). The IPC protocols
   are already address-space-agnostic; only the marshalling layer changes.
2. **SYS_SPAWN dynamic loading** remains a stub — user processes are spawned from
   kernel-embedded ELF images. Dynamic spawn will load binaries from the VFS.
3. **File data marshalling** uses heap pointers in message words (valid while
   services share the kernel address space). Large transfers should move to a
   shared-memory / grant mechanism once address spaces are split.
4. **Capability enforcement on services** (who may register a name / open a path)
   is not yet wired; the plumbing (`capability` module) exists for it.

## Phase 2 exit criterion

> Core services run as separate tasks communicating over IPC; the system boots
> to a shell that can list and read files served by the VFS.

**Met.** Init, Registry, VFS, and MemFS run as discrete IPC-communicating tasks;
the shell exposes file operations against the VFS; the boot self-test proves the
full path works headlessly.
