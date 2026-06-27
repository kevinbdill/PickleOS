# Phase 2 Design: Core OS Services

This document details the architectural design for Phase 2 of PICKLE OS, which moves
core services into isolated user-space processes with capability-based security.

## Overview

Phase 2 transforms PICKLE OS from a kernel with user-space support into a true microkernel
system where essential OS services run as unprivileged user processes. Each service is
isolated, capability-confined, and communicates via IPC.

## Architecture Components

```
┌─────────────────────────────────────────────────────────────┐
│                       PICKLE OS Kernel                          │
│  (scheduling, memory, IPC, capabilities, syscalls)          │
└────────────┬────────────────────────────────────────────────┘
             │
    ┌────────┴─────────┐
    │                  │
┌───▼────┐      ┌──────▼──────┐      ┌──────────────┐
│  Init  │─────▶│   Registry  │◀─────│  VFS Server  │
│ Server │      │   Server    │      │              │
└────────┘      └─────────────┘      └──────┬───────┘
                                             │
                                      ┌──────▼──────┐
                                      │   MemFS     │
                                      │  Backend    │
                                      └─────────────┘
                       │
              ┌────────┴────────┐
              │                 │
        ┌─────▼─────┐    ┌─────▼──────┐
        │User Shell │    │  Coreutils │
        │           │    │ (ls, cat)  │
        └───────────┘    └────────────┘
```

### 1. Init Server

**Purpose:** Bootstrap the user-space environment and distribute initial capabilities.

**Responsibilities:**
- First user process spawned by kernel
- Receives root capability table from kernel
- Spawns core system servers (Registry, VFS)
- Distributes capabilities to spawned services
- Acts as parent/reaper for all user processes

**Capabilities Granted:**
- Root capability table access
- Spawn capability (can create new processes)
- All endpoints for communication

### 2. Registry Server

**Purpose:** Provide name-to-capability resolution and service discovery.

**Responsibilities:**
- Maintains a mapping of service names to endpoint capabilities
- Allows services to register themselves under a name
- Provides lookup functionality for other processes
- Acts as the "naming service" for the system

**Protocol:**
```rust
enum RegistryRequest {
    Register { name: [u8; 32], endpoint: CapId },
    Lookup { name: [u8; 32] },
    List,
}

enum RegistryResponse {
    Success,
    Endpoint(CapId),
    ServiceList(Vec<[u8; 32]>),
    Error(ErrorCode),
}
```

### 3. VFS Server

**Purpose:** Provide unified filesystem interface with pluggable backends.

**Responsibilities:**
- Exposes file operations (open, read, write, close, stat, readdir)
- Routes operations to appropriate backend filesystems
- Maintains file descriptor table per client
- Enforces capability-based access control

**Protocol:**
```rust
enum VfsRequest {
    Open { path: [u8; 256], flags: u32 },
    Read { fd: u32, len: usize },
    Write { fd: u32, data: [u8; 4096] },
    Close { fd: u32 },
    Stat { path: [u8; 256] },
    ReadDir { path: [u8; 256] },
}

enum VfsResponse {
    Fd(u32),
    Data { data: [u8; 4096], len: usize },
    Written(usize),
    Success,
    StatInfo { size: u64, is_dir: bool },
    DirEntries { entries: Vec<[u8; 256]> },
    Error(ErrorCode),
}
```

### 4. MemFS Backend

**Purpose:** In-memory filesystem for initial testing.

**Features:**
- Tree-structured in-memory files and directories
- Supports basic operations: create, read, write, list
- No persistence (lost on reboot)
- Simple implementation to bootstrap VFS

### 5. User Library (`libpickleos`)

**Purpose:** Provide a Rust standard library for user-space programs.

**Features:**
- Syscall wrappers (safe Rust APIs)
- IPC helpers
- Basic data structures (Vec, String, HashMap via alloc)
- No dependency on std or libc
- Panic handler
- Memory allocator setup

**Module Structure:**
```
libkextos/
├── src/
│   ├── lib.rs           # Core library, no_std setup
│   ├── syscall.rs       # Raw syscall wrappers
│   ├── ipc.rs           # IPC abstractions
│   ├── capability.rs    # Capability helpers
│   ├── fs.rs            # File system abstractions
│   ├── process.rs       # Process management
│   └── alloc.rs         # Heap allocator
```

### 6. User Shell

**Purpose:** Interactive command-line interface for testing and interaction.

**Features:**
- Reads commands from keyboard
- Parses and executes built-in commands
- Spawns external programs
- Uses VFS for file operations

**Built-in Commands:**
- `help` - show available commands
- `ps` - list processes (via syscall)
- `ls [path]` - list directory
- `cat <file>` - show file contents
- `echo <text>` - print text
- `clear` - clear screen
- `exit` - exit shell

### 7. Coreutils

**Purpose:** Basic command-line utilities.

**Tools:**
- `ls` - list directory contents
- `cat` - concatenate and display files
- `echo` - print arguments
- `mkdir` - create directory
- `rm` - remove file
- `cp` - copy file

## Required Syscall Extensions

We need to add the following syscalls beyond Phase 1:

### Process Management

```rust
// Spawn a new user process from ELF binary in memory
SYS_SPAWN (13)
  Args: binary_ptr: *const u8, binary_len: usize, name_ptr: *const u8
  Returns: TaskId or error

// Exit with status code (enhanced version of existing SYS_EXIT)
SYS_EXIT_WITH_CODE (14)
  Args: exit_code: i32
  Returns: never
```

### Memory Management

```rust
// Map anonymous memory region
SYS_MMAP (15)
  Args: addr: usize, len: usize, prot: u32
  Returns: mapped address or error

// Unmap memory region
SYS_MUNMAP (16)
  Args: addr: usize, len: usize
  Returns: success or error

// Share memory with another task
SYS_SHARE_MEMORY (17)
  Args: target_task: TaskId, addr: usize, len: usize, prot: u32
  Returns: success or error
```

### Time Management

```rust
// Get current system tick count
SYS_GET_TICKS (18)
  Args: none
  Returns: u64 tick count

// Sleep for specified number of ticks
SYS_SLEEP (19)
  Args: ticks: u64
  Returns: success (after wake)
```

### Enhanced IPC

Existing IPC syscalls are sufficient, but we may add:

```rust
// Receive with timeout
SYS_IPC_RECV_TIMEOUT (20)
  Args: endpoint: CapId, timeout_ticks: u64
  Returns: Message or timeout error
```

## Implementation Plan

### Step 1: Extend Syscall Interface
- Add new syscall numbers and handlers
- Implement SYS_SPAWN, SYS_MMAP, SYS_GET_TICKS, SYS_SLEEP
- Test each syscall individually

### Step 2: Build User Library
- Create `libkextos` crate
- Implement syscall wrappers
- Add basic allocator
- Provide IPC abstractions

### Step 3: Implement Init Server
- Write init.rs in userspace/
- Spawn and initialize system
- Test init can spawn other processes

### Step 4: Implement Registry Server
- Write registry.rs
- Handle registration and lookup
- Test from init

### Step 5: Implement MemFS Backend
- Create in-memory file tree
- Implement file/directory operations
- Unit test the backend

### Step 6: Implement VFS Server
- Write vfs.rs
- Integrate MemFS backend
- Handle file operations over IPC
- Test basic file operations

### Step 7: Implement User Shell
- Write shell.rs
- Parse commands and execute
- Integrate with VFS for file commands
- Test interactively

### Step 8: Implement Coreutils
- Write ls, cat, echo, etc.
- Each as a separate binary
- Test via shell

### Step 9: Integration Testing
- Boot to shell automatically
- Verify file operations work end-to-end
- Confirm Phase 2 exit criterion

## Security Model

All services follow capability-based security:

1. **No ambient authority** - services cannot access resources without explicit capabilities
2. **Least privilege** - each service receives only the capabilities it needs
3. **Delegation** - services can pass capabilities to others (e.g., VFS to file backends)
4. **Revocation** - init can revoke capabilities if needed

Example capability flow:
```
Kernel → Init (root caps)
Init → Registry (spawn, registry endpoint)
Init → VFS (spawn, vfs endpoint, storage capabilities)
VFS → MemFS (memory allocation capabilities)
Shell → VFS (file operation capabilities via VFS endpoint)
```

## Testing Strategy

1. **Unit tests** - test each syscall individually
2. **Integration tests** - test service-to-service communication
3. **End-to-end tests** - boot to shell and run commands
4. **Capability tests** - verify services can't access resources without caps

## Success Criteria (Phase 2 Exit)

Phase 2 is complete when:
- [ ] Boot reaches a user-space shell (not kernel shell)
- [ ] Shell can list files in the root directory (served by VFS/MemFS)
- [ ] Shell can read and display file contents via `cat`
- [ ] All core services run in user space with capability confinement
- [ ] No regression in Phase 0/1 functionality

## Future Extensions (Phase 3+)

- Persistent filesystem backends (ext2, custom)
- Block device drivers in user space
- Network filesystem support
- Process isolation enforcement in MMU
- Copy-on-write memory sharing
