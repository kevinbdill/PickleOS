//! Tasks, the scheduler, and low-level context switching.
//!
//! A **task** is the unit of execution. Each task has its own kernel stack and
//! a saved stack pointer (`rsp`). Switching between tasks is done by
//! [`context_switch`], an assembly routine that saves the current task's
//! callee-saved registers + stack pointer and restores another task's.
//!
//! ## Why this is *real* preemptive multitasking
//! * The timer interrupt ([`crate::interrupts`]) calls [`scheduler::on_timer_tick`]
//!   on every tick, which can switch to another runnable task — tasks do not
//!   have to cooperate to be preempted.
//! * Tasks may also voluntarily [`yield_now`] (e.g. while idle) or block
//!   (e.g. waiting for IPC), in which case the scheduler runs the next task.
//!
//! ## Context-switch contract
//! `context_switch(old_rsp_ptr, new_rsp)` (System V: `rdi`, `rsi`):
//!   1. Pushes callee-saved regs (rbp, rbx, r12–r15) onto the *current* stack.
//!   2. Saves the resulting `rsp` into `*old_rsp_ptr`.
//!   3. Loads `rsp = new_rsp` (the other task's saved stack pointer).
//!   4. Pops its callee-saved regs and `ret`s — resuming exactly where that
//!      task last switched out (or, for a fresh task, the trampoline).
//!
//! Callers must hold interrupts disabled across the switch; each resume path is
//! responsible for re-enabling them appropriately.

use crate::gdt;
use crate::serial_println;
use crate::signal::{self, SavedSigContext, SignalState};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;
use x86_64::{PhysAddr, VirtAddr};

/// Each kernel task gets a 128 KiB stack. These stacks live on the kernel heap
/// (no hardware guard page), so an overflow silently corrupts adjacent heap
/// objects and wedges the kernel. Some tasks run deep, allocation-heavy call
/// chains — e.g. `ahci-init` walks `ahci::init → block::selftest →
/// nextfs_selftest → init_user::run`, each frame also formatting strings and
/// mapping page tables — so 32 KiB proved too tight and would overflow once
/// real disks were attached. 128 KiB gives a comfortable margin.
const KSTACK_SIZE: usize = 128 * 1024;

/// Monotonic task-ID counter.
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// Id of the `init` task (the orphan reaper, conventionally PID 1). Set once at
/// boot via [`set_init_task`]. `u64::MAX` means "not yet registered", in which
/// case orphaned children are simply left as zombies until shutdown.
static INIT_TASK_ID: AtomicU64 = AtomicU64::new(u64::MAX);

/// Register the task that should adopt orphaned children (the reaper / `init`).
pub fn set_init_task(id: u64) {
    INIT_TASK_ID.store(id, Ordering::SeqCst);
}

/// The registered init/reaper task id, if any.
pub fn init_task_id() -> Option<u64> {
    let v = INIT_TASK_ID.load(Ordering::SeqCst);
    if v == u64::MAX {
        None
    } else {
        Some(v)
    }
}

/// Lifecycle state of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Ready to run, waiting for a CPU slice.
    Runnable,
    /// Currently executing on the CPU.
    Running,
    /// Waiting for an event (e.g. an IPC message, or a child to exit); not
    /// schedulable until woken.
    Blocked,
    /// Terminated but not yet reaped by its parent. The exit status is retained
    /// in [`Task::exit_status`] until a `wait()` collects it. Not schedulable.
    Zombie,
    /// Finished and reaped (or a kernel task that simply ended). Inert; the slot
    /// lingers in the task table but is never scheduled again.
    Dead,
}

/// Privilege level of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ring {
    /// Kernel-mode task (ring 0).
    Kernel,
    /// User-mode task (ring 3).
    User,
}

/// A schedulable task. The kernel stack is heap-allocated and kept alive by the
/// `Box` so its address (and therefore the saved `rsp`) stays valid.
pub struct Task {
    pub id: u64,
    pub name: String,
    /// Saved kernel stack pointer (updated by `context_switch`).
    pub rsp: u64,
    /// Top (highest address) of this task's kernel stack, loaded into the TSS
    /// so interrupts/syscalls taken while this task runs use the right stack.
    pub kstack_top: VirtAddr,
    pub state: State,
    /// Backing storage for the kernel stack (owned to keep it alive).
    _stack: Box<[u8]>,
    /// Privilege level: kernel (ring 0) or user (ring 3).
    pub ring: Ring,
    /// User-space page table (CR3 physical address). Only valid for ring 3 tasks.
    pub user_cr3: Option<PhysAddr>,
    /// User-space stack pointer. Only used for ring 3 tasks.
    pub user_rsp: u64,
    // --- Process-tree bookkeeping ------------------------------------------
    /// Parent task id, or `None` for the root tasks created during boot.
    pub parent: Option<u64>,
    /// Ids of child tasks spawned via `fork`. Entries are removed as children
    /// are reaped by `wait`.
    pub children: Vec<u64>,
    /// Exit status set when the task calls `exit`; meaningful once the task is
    /// in the [`State::Zombie`] (or `Dead`) state.
    pub exit_status: i32,
    /// True while this task is blocked specifically inside `wait()` for a child
    /// to exit. Distinguishes a `wait` sleep from an IPC sleep so `exit` only
    /// wakes parents that are actually waiting on children.
    pub wait_blocked: bool,
    /// Per-task signal state: pending signals, handler dispositions, and the
    /// saved context used by `SYS_SIGRETURN`.
    pub signals: SignalState,
    /// Next free user virtual address for `mmap` allocations in this task's
    /// private address space. Bumps upward as the task maps regions (e.g. its
    /// heap). Starts at [`crate::memory::USER_HEAP_START`].
    pub mmap_next: u64,
}

// ---------------------------------------------------------------------------
// Low-level context switch + new-task trampoline (assembly).
// ---------------------------------------------------------------------------
global_asm!(
    r#"
.global context_switch
context_switch:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov [rdi], rsp        # *old_rsp_ptr = rsp
    mov rsp, rsi          # rsp = new_rsp
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret                   # resume the other task

.global task_trampoline
task_trampoline:
    sti                   # new tasks start with interrupts enabled
    call r12              # call the task entry fn (pointer placed in r12 slot)
    call task_exit_current  # if the entry ever returns, reap this task

.global fork_child_trampoline
fork_child_trampoline:
    # A forked child is first entered here via `ret` from context_switch (which
    # already popped this task's callee-saved registers). RSP now points at the
    # child's crafted SyscallFrame (15 GP regs, r15 lowest) immediately followed
    # by the iret frame. We replay exactly the `syscall_stub` epilogue: pop all
    # general-purpose registers (rax was crafted to 0 so fork returns 0 in the
    # child) and `iretq` back to ring 3 at the instruction after the child's
    # original `int 0x80`. The child's CR3 was already loaded by the scheduler
    # before the switch, so the iret frame and user pages are in scope.
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbp
    pop rbx
    pop r11
    pop rcx
    pop r9
    pop r8
    pop r10
    pop rdx
    pop rsi
    pop rdi
    pop rax
    iretq
"#
);

extern "C" {
    /// See the `global_asm!` block above.
    fn context_switch(old_rsp_ptr: *mut u64, new_rsp: u64);
    fn task_trampoline();
    /// Entry stub for a freshly forked child (see the asm block above).
    fn fork_child_trampoline();
}

/// Called by the trampoline if a task's entry function ever returns. Marks the
/// task dead and schedules away; never returns.
#[no_mangle]
extern "C" fn task_exit_current() -> ! {
    x86_64::instructions::interrupts::disable();
    scheduler::with(|s| {
        let cur = s.current;
        s.tasks[cur].state = State::Dead;
    });
    scheduler::schedule();
    // We marked ourselves Dead, so schedule() will never return here.
    unreachable!("dead task resumed");
}

impl Task {
    /// Create a new kernel task that begins executing `entry`.
    ///
    /// We hand-craft the initial stack so that the very first `context_switch`
    /// into this task "returns" into [`task_trampoline`], which then calls
    /// `entry`. The entry function pointer is smuggled in via the `r12` slot.
    fn new(name: &str, entry: extern "C" fn() -> !) -> Task {
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let stack: Box<[u8]> = vec![0u8; KSTACK_SIZE].into_boxed_slice();

        // 16-byte align the top so the System V ABI stack alignment holds at
        // the entry of `entry` (rsp % 16 == 8 right after the trampoline's
        // `call`, which is what the ABI expects).
        let base = stack.as_ptr() as u64;
        let top = (base + KSTACK_SIZE as u64) & !0xF;

        // Lay out the initial saved context (see context-switch contract):
        //   [top-1] = return address  -> task_trampoline
        //   [top-2] = rbp (0)
        //   [top-3] = rbx (0)
        //   [top-4] = r12 -> entry fn pointer (read by the trampoline)
        //   [top-5] = r13 (0)
        //   [top-6] = r14 (0)
        //   [top-7] = r15 (0)  <- initial rsp points here
        let sp = top as *mut u64;
        unsafe {
            *sp.offset(-1) = task_trampoline as *const () as u64;
            *sp.offset(-2) = 0; // rbp
            *sp.offset(-3) = 0; // rbx
            *sp.offset(-4) = entry as *const () as u64; // r12 (carries the entry pointer)
            *sp.offset(-5) = 0; // r13
            *sp.offset(-6) = 0; // r14
            *sp.offset(-7) = 0; // r15
        }
        let rsp = unsafe { sp.offset(-7) } as u64;

        Task {
            id,
            name: String::from(name),
            rsp,
            kstack_top: VirtAddr::new(top),
            state: State::Runnable,
            _stack: stack,
            ring: Ring::Kernel,
            user_cr3: None,
            user_rsp: 0,
            parent: None,
            children: Vec::new(),
            exit_status: 0,
            wait_blocked: false,
            signals: SignalState::new(),
            mmap_next: crate::memory::USER_HEAP_START,
        }
    }
}

/// Spawn a new kernel task and add it to the run queue. Safe to call both
/// during init (before the scheduler runs) and from a running task.
pub fn spawn_kernel_task(name: &str, entry: extern "C" fn() -> !) -> u64 {
    let task = Task::new(name, entry);
    let id = task.id;
    crate::fs::init_task_fds(id as u32);
    scheduler::with(|s| s.tasks.push(Box::new(task)));
    serial_println!("task :: spawned '{}' (id {})", name, id);
    id
}

/// Spawn a user-space task from an ELF binary.
///
/// This loads the ELF into a fresh address space, creates a task with Ring::User,
/// and sets up the initial user stack and entry point for ring 3 execution.
pub fn spawn_user_task(name: &str, elf_data: &'static [u8]) -> Result<u64, &'static str> {
    use crate::elf::ElfBinary;

    // Parse the ELF binary.
    let elf = ElfBinary::parse(elf_data)?;
    
    // Load it into a fresh user address space and get the entry point + stack.
    // The program name becomes argv[0]; no environment by default.
    let (entry, user_stack_top) = elf.load(elf_data, &[name], &[])?;

    spawn_user_from_entry(name, entry, user_stack_top)
}

/// Spawn a user-space task from an ELF binary stored on the filesystem.
///
/// Uses [`crate::elf::load_from_file`] to read and load the program from
/// NextFS, then crafts the ring-3 task exactly as [`spawn_user_task`] does.
/// This is the path the user-space `init` uses to launch programs from disk.
pub fn spawn_user_task_from_file(name: &str, path: &str) -> Result<u64, &'static str> {
    // Seed argv[0] with the program path (conventional), no environment.
    let (entry, user_stack_top) = crate::elf::load_from_file(path, &[path], &[])?;
    spawn_user_from_entry(name, entry, user_stack_top)
}

/// Like [`spawn_user_task_from_file`] but with an explicit argument and
/// environment vector seeded onto the new program's initial stack.
pub fn spawn_user_task_from_file_args(
    name: &str,
    path: &str,
    args: &[&str],
    envs: &[&str],
) -> Result<u64, &'static str> {
    let (entry, user_stack_top) = crate::elf::load_from_file(path, args, envs)?;
    spawn_user_from_entry(name, entry, user_stack_top)
}

/// Craft and enqueue a ring-3 task given an already-loaded entry point and
/// user stack top (the address space must already be populated by the ELF
/// loader, with its page table registered as the most recent user CR3).
fn spawn_user_from_entry(
    name: &str,
    entry: VirtAddr,
    user_stack_top: VirtAddr,
) -> Result<u64, &'static str> {
    serial_println!("task :: user task '{}' loaded at entry {:#x}", name, entry.as_u64());

    // Create a task structure (with a kernel stack for syscalls/interrupts).
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    let stack: Box<[u8]> = vec![0u8; KSTACK_SIZE].into_boxed_slice();
    let base = stack.as_ptr() as u64;
    let kstack_top = (base + KSTACK_SIZE as u64) & !0xF;

    // For a user task, we craft a kernel stack so that when context_switch
    // "returns" into this task, it jumps to the trampoline, which then irets to ring 3.
    //
    // Stack layout (from high to low addresses):
    //   [kstack_top - 8*1]  = SS (user data segment)
    //   [kstack_top - 8*2]  = user RSP
    //   [kstack_top - 8*3]  = RFLAGS
    //   [kstack_top - 8*4]  = CS (user code segment)
    //   [kstack_top - 8*5]  = RIP (entry point)        <-- trampoline expects RSP here
    //   [kstack_top - 8*6]  = return address (trampoline)
    //   [kstack_top - 8*7]  = rbp (callee-saved)
    //   [kstack_top - 8*8]  = rbx
    //   [kstack_top - 8*9]  = r12
    //   [kstack_top - 8*10] = r13
    //   [kstack_top - 8*11] = r14
    //   [kstack_top - 8*12] = r15                      <-- initial RSP points here

    let sp = kstack_top as *mut u64;
    // User segments must have RPL=3 (bits 0-1 of the selector).
    let user_cs = (crate::gdt::selectors().user_code_selector.0 as u64) | 3;
    let user_ss = (crate::gdt::selectors().user_data_selector.0 as u64) | 3;
    // RFLAGS: bit 1 (reserved, always 1) | bit 9 (IF, interrupts enabled)
    let rflags: u64 = 0x202;
    
    // Get the user CR3 so we can pass it via r12 and store it in the Task struct
    let user_cr3 = crate::memory::with_memory(|mem| {
        mem.user_page_tables
            .last()
            .copied()
            .ok_or("no user page table created")
    })?;

    unsafe {
        // Build iret frame (what iretq will consume)
        *sp.offset(-1) = user_ss;                 // SS
        *sp.offset(-2) = user_stack_top.as_u64(); // user RSP
        *sp.offset(-3) = rflags;                  // RFLAGS
        *sp.offset(-4) = user_cs;                 // CS
        *sp.offset(-5) = entry.as_u64();          // RIP
        
        // Return address: when context_switch pops callee-saved regs and executes ret,
        // it will jump here (the trampoline, which then does iretq).
        *sp.offset(-6) = user_task_trampoline as *const () as u64;
        
        // Callee-saved registers (context_switch will pop these before ret)
        *sp.offset(-7) = sp.offset(-5) as u64;  // rbp = pointer to iret frame (for trampoline)
        *sp.offset(-8) = 0;  // rbx
        *sp.offset(-9) = user_cr3.as_u64();  // r12 = user CR3 (trampoline will load it)
        *sp.offset(-10) = 0; // r13
        *sp.offset(-11) = 0; // r14
        *sp.offset(-12) = 0; // r15
    }
    
    // Initial RSP: at the bottom of the callee-saved area
    let rsp = unsafe { sp.offset(-12) } as u64;

    let task = Task {
        id,
        name: String::from(name),
        rsp,
        kstack_top: VirtAddr::new(kstack_top),
        state: State::Runnable,
        _stack: stack,
        ring: Ring::User,
        user_cr3: Some(user_cr3), // Stored for reference, but trampoline will load from r12
        user_rsp: user_stack_top.as_u64(),
        parent: None,
        children: Vec::new(),
        exit_status: 0,
        wait_blocked: false,
        signals: SignalState::new(),
        mmap_next: crate::memory::USER_HEAP_START,
    };

    let task_id = task.id;
    crate::fs::init_task_fds(task_id as u32);
    scheduler::with(|s| s.tasks.push(Box::new(task)));
    serial_println!("task :: spawned user task '{}' (id {})", name, task_id);

    Ok(task_id)
}

/// Trampoline for user tasks: loads user CR3 and transitions to ring 3 via iretq.
///
/// The initial kernel stack (crafted by spawn_user_task) has an iret frame
/// ready. When we arrive here (via ret from context_switch), r12 contains
/// the user CR3 physical address, and RSP points to the iret frame.
#[unsafe(naked)]
extern "C" fn user_task_trampoline() -> ! {
    core::arch::naked_asm!(
        ".intel_syntax noprefix",
        // Load user CR3 from r12 (context_switch restored it from the task's stack)
        "mov cr3, r12",
        // Now execute iretq to ring 3 (RSP already points to the iret frame)
        "iretq"
    );
}

// ===========================================================================
// Process management: exit / wait / exec / fork.
//
// These implement the POSIX-flavoured process primitives on top of the task
// table. A "process" here is simply a ring-3 task plus its address space, file
// descriptors and process-tree links (parent / children / exit status).
// ===========================================================================

/// Terminate the current task with `status`.
///
/// Marks the task a [`State::Zombie`] (retaining `status` for the parent's
/// `wait`), closes its file descriptors, reparents any surviving children to
/// the init/reaper task, wakes the parent if it is blocked in `wait`, and
/// schedules away. Never returns.
pub fn do_exit(status: i32) -> ! {
    x86_64::instructions::interrupts::disable();

    let (cur_id, children, parent) = scheduler::with(|s| {
        let cur = s.current;
        let t = &mut s.tasks[cur];
        t.exit_status = status;
        t.state = State::Zombie;
        let children = core::mem::take(&mut t.children);
        (t.id, children, t.parent)
    });

    serial_println!("[exit] task {} exited with status {}", cur_id, status);

    // Release the exiting task's open files and credentials.
    crate::fs::cleanup_task_fds(cur_id as u32);

    // Tear down any window-server windows this task owned. Crucial when a task
    // is killed by a fault (its user-space `Window::drop` never runs), so the
    // compositor would otherwise keep compositing/polling a dead client.
    let _ = crate::wm::destroy_windows_owned_by(cur_id);

    // Reparent surviving children to the reaper (init), if registered.
    let reaper = init_task_id();
    if !children.is_empty() {
        scheduler::with(|s| {
            for &child in &children {
                if let Some(t) = s.tasks.iter_mut().find(|t| t.id == child) {
                    t.parent = reaper;
                }
            }
            if let Some(rid) = reaper {
                if let Some(init_t) = s.tasks.iter_mut().find(|t| t.id == rid) {
                    init_t.children.extend_from_slice(&children);
                }
            }
        });
    }

    // Notify the parent: post SIGCHLD and wake it if it is blocked in wait().
    if let Some(pid) = parent {
        scheduler::with(|s| {
            if let Some(t) = s.tasks.iter_mut().find(|t| t.id == pid) {
                // A terminating child posts SIGCHLD to its parent. The default
                // disposition is "ignore", but a parent that installed a
                // handler will have it delivered at its next syscall boundary.
                t.signals.set_pending(signal::SIGCHLD);
                if t.state == State::Blocked && t.wait_blocked {
                    t.state = State::Runnable;
                }
            }
        });
    }

    scheduler::schedule();
    unreachable!("exited (zombie) task resumed");
}

/// Result of scanning the current task's children in [`do_wait`].
enum WaitScan {
    /// The task has no children to wait for.
    NoChildren,
    /// A terminated child was found: (child id, exit status).
    Reaped(u64, i32),
    /// Children exist but none have terminated yet — block and retry.
    WaitMore,
}

/// Wait for any child of the current task to terminate.
///
/// Wait for a child to exit. If `status_ptr` is non-zero and points to valid
/// user-space memory, the child's exit status (an `i32`) is written there.
/// Returns `u64::MAX` if the task has no children.
///
/// The caller (syscall layer) is responsible for validating `status_ptr`.
pub fn do_wait(status_ptr: u64) -> u64 {
    loop {
        let scan = scheduler::with(|s| {
            let cur = s.current;
            if s.tasks[cur].children.is_empty() {
                return WaitScan::NoChildren;
            }
            let child_ids = s.tasks[cur].children.clone();
            for cid in child_ids {
                if let Some(ct) = s.tasks.iter().find(|t| t.id == cid) {
                    if ct.state == State::Zombie {
                        return WaitScan::Reaped(cid, ct.exit_status);
                    }
                }
            }
            WaitScan::WaitMore
        });

        match scan {
            WaitScan::NoChildren => return u64::MAX,
            WaitScan::Reaped(cid, status) => {
                // Remove the child from our list and retire its slot.
                scheduler::with(|s| {
                    let cur = s.current;
                    s.tasks[cur].children.retain(|&c| c != cid);
                    if let Some(ct) = s.tasks.iter_mut().find(|t| t.id == cid) {
                        ct.state = State::Dead;
                    }
                });
                crate::fs::cleanup_task_fds(cid as u32);
                if status_ptr != 0 {
                    // Validate that status_ptr points to valid user memory
                    // before writing through it.
                    if status_ptr < 0x0000_8000_0000_0000
                        && status_ptr.checked_add(4).map_or(false, |end| end <= 0x0000_8000_0000_0000)
                    {
                        unsafe {
                            *(status_ptr as *mut i32) = status;
                        }
                    }
                }
                serial_println!("[wait] reaped child {} (status {})", cid, status);
                return cid;
            }
            WaitScan::WaitMore => {
                // No child ready: block until one exits. Interrupts are already
                // disabled in syscall context (interrupt gate), which satisfies
                // schedule()'s precondition; we leave them disabled across the
                // sleep and only the eventual iretq restores the caller's IF.
                x86_64::instructions::interrupts::disable();
                scheduler::with(|s| {
                    let cur = s.current;
                    s.tasks[cur].wait_blocked = true;
                    s.tasks[cur].state = State::Blocked;
                });
                scheduler::schedule();
                scheduler::with(|s| {
                    let cur = s.current;
                    s.tasks[cur].wait_blocked = false;
                });
            }
        }
    }
}

/// Replace the current task's program image with the ELF at `path`.
///
/// Loads a fresh address space from NextFS, rewrites the saved syscall frame so
/// the syscall "returns" straight into the new program's entry point on a fresh
/// stack, and switches CR3 to the new space. The PID, parent link, children and
/// open file descriptors are all preserved. Returns `u64::MAX` on failure (the
/// caller's image is left intact); on success it does not conceptually return
/// to the old image.
pub fn do_exec(
    frame: &mut crate::syscall::SyscallFrame,
    path: &str,
    args: &[alloc::string::String],
    envs: &[alloc::string::String],
) -> u64 {
    // Build borrowed slices for the loader. If no argv was supplied, default
    // argv[0] to the program path so programs always see a valid argv[0].
    let path_arg = [path];
    let args_ref: Vec<&str> = if args.is_empty() {
        path_arg.to_vec()
    } else {
        args.iter().map(|s| s.as_str()).collect()
    };
    let envs_ref: Vec<&str> = envs.iter().map(|s| s.as_str()).collect();

    // Load the new program into a brand-new user address space.
    //
    // We arrive here through the `int 0x80` interrupt gate, which clears IF, so
    // interrupts are currently disabled. Reading the ELF off NextFS goes through
    // the AHCI driver, whose `wait_not_busy`/completion polling cooperatively
    // yields and depends on the device making progress; this only works
    // reliably with interrupts enabled (the same context `init` loads programs
    // in). Re-enable interrupts for the duration of the disk read, then restore
    // the disabled state so the frame fix-up and CR3 switch below run atomically.
    let irq_were_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::enable();
    let load_result = crate::elf::load_from_file(path, &args_ref, &envs_ref);
    if !irq_were_enabled {
        x86_64::instructions::interrupts::disable();
    }
    let (entry, user_stack_top) = match load_result {
        Ok(v) => v,
        Err(e) => {
            serial_println!("[exec] failed to load '{}': {}", path, e);
            return u64::MAX;
        }
    };

    // The freshly created mapper is the most-recent entry in user_page_tables.
    let new_cr3 = match crate::memory::with_memory(|mem| mem.user_page_tables.last().copied()) {
        Some(c) => c,
        None => {
            serial_println!("[exec] no user page table after load");
            return u64::MAX;
        }
    };

    // Update the task's recorded address space + user stack.
    let cur_id = scheduler::with(|s| {
        let cur = s.current;
        s.tasks[cur].user_cr3 = Some(new_cr3);
        s.tasks[cur].user_rsp = user_stack_top.as_u64();
        // execve resets all signal handlers to their default disposition.
        s.tasks[cur].signals.reset_for_exec();
        s.tasks[cur].id
    });

    let user_cs = (crate::gdt::selectors().user_code_selector.0 as u64) | 3;
    let user_ss = (crate::gdt::selectors().user_data_selector.0 as u64) | 3;
    let rflags: u64 = 0x202;

    // Zero the general-purpose registers (a fresh process starts clean).
    frame.r15 = 0;
    frame.r14 = 0;
    frame.r13 = 0;
    frame.r12 = 0;
    frame.rbp = 0;
    frame.rbx = 0;
    frame.r11 = 0;
    frame.rcx = 0;
    frame.r9 = 0;
    frame.r8 = 0;
    frame.r10 = 0;
    frame.rdx = 0;
    frame.rsi = 0;
    frame.rdi = 0;
    frame.rax = 0;

    // Rewrite the CPU iret frame (located just above the SyscallFrame on the
    // kernel stack) so the syscall return path jumps to the new entry point.
    let frame_base = frame as *mut crate::syscall::SyscallFrame as *mut u64;
    unsafe {
        *frame_base.add(15) = entry.as_u64(); // RIP
        *frame_base.add(16) = user_cs; // CS
        *frame_base.add(17) = rflags; // RFLAGS
        *frame_base.add(18) = user_stack_top.as_u64(); // RSP
        *frame_base.add(19) = user_ss; // SS
    }

    serial_println!(
        "[exec] task {} exec'd '{}' (entry {:#x}, cr3 {:#x})",
        cur_id, path, entry.as_u64(), new_cr3.as_u64()
    );

    // Switch to the new address space immediately. The syscall epilogue + iretq
    // run before the next reschedule, so CR3 must already point at the space
    // that maps the new entry/stack. Kernel mappings are shared across every
    // address space, so the in-flight kernel stack stays valid.
    unsafe {
        core::arch::asm!(
            "mov cr3, {}",
            in(reg) new_cr3.as_u64(),
            options(nostack, preserves_flags)
        );
    }

    0
}

/// Create a child process that is a copy of the current (calling) task.
///
/// Duplicates the parent's user address space (deep copy — no copy-on-write
/// yet), clones its file descriptors and credentials, links the child into the
/// process tree, and crafts the child's kernel stack so that when first
/// scheduled it returns from this very syscall with `0` in `rax`. Returns the
/// new child's id to the parent, or `u64::MAX` on failure.
pub fn do_fork(frame: &crate::syscall::SyscallFrame) -> u64 {
    // Snapshot parent identity + address space.
    let (parent_id, parent_cr3) = scheduler::with(|s| {
        let cur = s.current;
        (s.tasks[cur].id, s.tasks[cur].user_cr3)
    });
    let parent_cr3 = match parent_cr3 {
        Some(c) => c,
        None => {
            serial_println!("[fork] refused: caller is not a user task");
            return u64::MAX;
        }
    };

    // 1. Create the child's address space via Copy-on-Write.
    //    All writable user pages are shared read-only + COW between parent and child.
    //    The parent's TLB is flushed for each modified PTE so it sees the new
    //    permissions immediately. A private copy is made only when either process
    //    writes to a shared page (resolved in the page fault handler).
    let child_cr3 = crate::memory::with_memory(|mem| mem.cow_fork_address_space(parent_cr3));

    // 2. Allocate the child's kernel stack.
    let child_id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    let stack: Box<[u8]> = vec![0u8; KSTACK_SIZE].into_boxed_slice();
    let base = stack.as_ptr() as u64;
    let kstack_top = (base + KSTACK_SIZE as u64) & !0xF;

    // Read the parent's CPU iret frame (just above the SyscallFrame).
    let frame_base = frame as *const crate::syscall::SyscallFrame as *const u64;
    let (rip, cs, rflags, user_rsp, ss) = unsafe {
        (
            *frame_base.add(15),
            *frame_base.add(16),
            *frame_base.add(17),
            *frame_base.add(18),
            *frame_base.add(19),
        )
    };

    // 3. Craft the child kernel stack (see fork_child_trampoline for the layout
    //    contract). High → low: iret frame, SyscallFrame (rax = 0 so the child
    //    sees fork()==0), trampoline return address, 6 callee-saved slots.
    let sp = kstack_top as *mut u64;
    unsafe {
        // iret frame
        *sp.offset(-1) = ss;
        *sp.offset(-2) = user_rsp;
        *sp.offset(-3) = rflags;
        *sp.offset(-4) = cs;
        *sp.offset(-5) = rip;
        // SyscallFrame: rax (highest) down to r15 (lowest).
        *sp.offset(-6) = 0; // rax -> child returns 0
        *sp.offset(-7) = frame.rdi;
        *sp.offset(-8) = frame.rsi;
        *sp.offset(-9) = frame.rdx;
        *sp.offset(-10) = frame.r10;
        *sp.offset(-11) = frame.r8;
        *sp.offset(-12) = frame.r9;
        *sp.offset(-13) = frame.rcx;
        *sp.offset(-14) = frame.r11;
        *sp.offset(-15) = frame.rbx;
        *sp.offset(-16) = frame.rbp;
        *sp.offset(-17) = frame.r12;
        *sp.offset(-18) = frame.r13;
        *sp.offset(-19) = frame.r14;
        *sp.offset(-20) = frame.r15;
        // context_switch `ret` target.
        *sp.offset(-21) = fork_child_trampoline as *const () as u64;
        // 6 callee-saved slots consumed by context_switch (popped r15..rbp in
        // that order from the lowest address). Values are irrelevant; the
        // trampoline reloads all registers from the SyscallFrame above.
        *sp.offset(-22) = 0; // rbp
        *sp.offset(-23) = 0; // rbx
        *sp.offset(-24) = 0; // r12
        *sp.offset(-25) = 0; // r13
        *sp.offset(-26) = 0; // r14
        *sp.offset(-27) = 0; // r15
    }
    let child_rsp = unsafe { sp.offset(-27) } as u64;

    // The child inherits the parent's signal dispositions/handlers but starts
    // with an empty pending set and no in-flight handler (POSIX semantics).
    let mut child_signals = scheduler::with(|s| s.tasks[s.current].signals.clone());
    child_signals.pending = 0;
    child_signals.saved = None;

    // 4. Build the child Task and link it into the process tree.
    let child = Task {
        id: child_id,
        name: alloc::format!("{}-child", scheduler::with(|s| s.tasks[s.current].name.clone())),
        rsp: child_rsp,
        kstack_top: VirtAddr::new(kstack_top),
        state: State::Runnable,
        _stack: stack,
        ring: Ring::User,
        user_cr3: Some(child_cr3),
        user_rsp,
        parent: Some(parent_id),
        children: Vec::new(),
        exit_status: 0,
        wait_blocked: false,
        signals: child_signals,
        mmap_next: crate::memory::USER_HEAP_START,
    };

    // 5. Clone the parent's fds + credentials for the child.
    crate::fs::clone_task_fds(parent_id as u32, child_id as u32);

    // 6. Register the child and record it on the parent.
    scheduler::with(|s| {
        s.tasks.push(Box::new(child));
        if let Some(p) = s.tasks.iter_mut().find(|t| t.id == parent_id) {
            p.children.push(child_id);
        }
    });

    serial_println!("[fork] task {} forked child {}", parent_id, child_id);

    // Parent path: return the child's id.
    child_id
}

// ===========================================================================
// Signals: kill / signal-disposition / delivery / sigreturn.
// ===========================================================================

/// The parent process id of the current task, or 0 if it has no parent
/// (the root/boot tasks). Backs the `SYS_GETPPID` syscall.
pub fn current_ppid() -> u64 {
    scheduler::with(|s| s.tasks[s.current].parent.unwrap_or(0))
}

/// Terminate an arbitrary task `id` with `status` (used for signal default
/// actions targeting another process). If `id` is the current task this defers
/// to [`do_exit`]; otherwise the target is turned into a zombie in place: its
/// fds are released, its children re-parented to init, and a waiting parent is
/// woken. Returns `true` if a live task was terminated.
fn terminate_task(id: u64, status: i32) -> bool {
    // Self-termination uses the normal exit path (it never returns).
    if id == current_id() {
        do_exit(status);
    }

    let (found, children, parent) = scheduler::with(|s| {
        if let Some(t) = s.tasks.iter_mut().find(|t| t.id == id) {
            if t.state == State::Zombie || t.state == State::Dead {
                return (false, Vec::new(), None);
            }
            t.exit_status = status;
            t.state = State::Zombie;
            let children = core::mem::take(&mut t.children);
            (true, children, t.parent)
        } else {
            (false, Vec::new(), None)
        }
    });

    if !found {
        return false;
    }

    crate::fs::cleanup_task_fds(id as u32);

    // Re-parent surviving children to the reaper (init).
    let reaper = init_task_id();
    if !children.is_empty() {
        scheduler::with(|s| {
            for &child in &children {
                if let Some(t) = s.tasks.iter_mut().find(|t| t.id == child) {
                    t.parent = reaper;
                }
            }
            if let Some(rid) = reaper {
                if let Some(init_t) = s.tasks.iter_mut().find(|t| t.id == rid) {
                    init_t.children.extend_from_slice(&children);
                }
            }
        });
    }

    // Notify the parent (SIGCHLD + wake if waiting).
    if let Some(pid) = parent {
        scheduler::with(|s| {
            if let Some(t) = s.tasks.iter_mut().find(|t| t.id == pid) {
                t.signals.set_pending(signal::SIGCHLD);
                if t.state == State::Blocked && t.wait_blocked {
                    t.state = State::Runnable;
                }
            }
        });
    }

    serial_println!("[signal] terminated task {} with status {}", id, status);
    true
}

/// Implement `kill(pid, sig)`: send signal `sig` to task `pid`.
///
/// Behaviour depends on the target's disposition for `sig`:
///   * `SIGKILL` always terminates (cannot be caught or ignored).
///   * Default disposition: apply the default action (terminate or ignore).
///   * Ignored (`SIG_IGN`): discard.
///   * A custom handler is installed: mark the signal pending so it is
///     delivered at the target's next syscall return.
///
/// `sig == 0` performs an existence check (returns success iff the target
/// exists). Returns 0 on success, `u64::MAX` on error (no such process or bad
/// signal number).
pub fn do_kill(pid: u64, sig: u32) -> u64 {
    if sig as usize >= signal::NSIG {
        return u64::MAX;
    }

    // Permission check: a user task may only kill itself, its own children,
    // or (if it is init/PID 1) any task. Kernel tasks bypass this check
    // (they are trusted). This prevents an arbitrary user task from killing
    // init or unrelated processes.
    let current = scheduler::with(|s| {
        let cur = s.current;
        (cur, s.tasks[cur].id, s.tasks[cur].ring)
    });
    let (cur_idx, cur_id, cur_ring) = current;
    if cur_ring == Ring::User {
        if cur_id != pid {
            // Not self — check if target is a child.
            let is_child = scheduler::with(|s| {
                s.tasks[cur_idx].children.contains(&pid)
                    || s.tasks.iter().any(|t| t.id == pid && t.parent == Some(cur_id))
            });
            let is_init = cur_id == init_task_id().unwrap_or(u64::MAX);
            if !is_child && !is_init {
                return u64::MAX; // permission denied
            }
        }
    }

    // Look up the target's existence and its disposition for this signal.
    let info = scheduler::with(|s| {
        s.tasks.iter().find(|t| t.id == pid).map(|t| {
            (
                t.state,
                t.ring,
                if (sig as usize) < signal::NSIG {
                    t.signals.handlers[sig as usize]
                } else {
                    signal::SIG_DFL
                },
            )
        })
    });

    let (state, _ring, handler) = match info {
        Some(v) => v,
        None => return u64::MAX, // no such process
    };

    // A signal to an already-dead/zombie task is a no-op success.
    if state == State::Zombie || state == State::Dead {
        return 0;
    }

    // sig 0: existence probe only.
    if sig == 0 {
        return 0;
    }

    // SIGKILL is uncatchable and unignorable.
    if sig == signal::SIGKILL {
        terminate_task(pid, 128 + sig as i32);
        return 0;
    }

    if handler == signal::SIG_IGN {
        return 0; // explicitly ignored
    }

    if handler == signal::SIG_DFL {
        if signal::default_terminates(sig) {
            terminate_task(pid, 128 + sig as i32);
        }
        // else: default action is ignore (e.g. SIGCHLD) -> nothing to do.
        return 0;
    }

    // A custom handler is installed: mark pending for delivery at the target's
    // next syscall boundary.
    scheduler::with(|s| {
        if let Some(t) = s.tasks.iter_mut().find(|t| t.id == pid) {
            t.signals.set_pending(sig);
            // Wake a sleeper so it can reach a syscall boundary and be served.
            if t.state == State::Blocked && t.wait_blocked {
                t.state = State::Runnable;
            }
        }
    });
    0
}

/// Implement `signal(sig, handler, restorer)`: install a disposition for `sig`.
///
/// `handler` is the user handler address, or [`signal::SIG_DFL`] /
/// [`signal::SIG_IGN`]. `restorer` is the address of the user-space trampoline
/// that issues `SYS_SIGRETURN` when the handler returns. `SIGKILL` cannot be
/// caught or ignored. Returns the previous handler value, or `u64::MAX` on
/// error.
pub fn do_signal(sig: u32, handler: u64, restorer: u64) -> u64 {
    if sig == 0 || sig as usize >= signal::NSIG {
        return u64::MAX;
    }
    if sig == signal::SIGKILL {
        return u64::MAX; // uncatchable
    }
    scheduler::with(|s| {
        let cur = s.current;
        let prev = s.tasks[cur].signals.handlers[sig as usize];
        s.tasks[cur].signals.handlers[sig as usize] = handler;
        if restorer != 0 {
            s.tasks[cur].signals.restorer = restorer;
        }
        prev
    })
}

/// Deliver any pending, caught signal to the current task by rewriting its trap
/// frame so the kernel returns into the user signal handler instead of the
/// interrupted instruction.
///
/// `syscall_ret` is the value the in-flight syscall would otherwise return in
/// `rax`; it is saved so `SYS_SIGRETURN` can restore it. Called at the end of
/// the syscall dispatcher for the current (user) task.
pub fn deliver_pending_signals(frame: &mut crate::syscall::SyscallFrame, syscall_ret: u64) {
    // Gather what (if anything) to deliver. Only user tasks with a custom
    // handler and no handler already in flight are eligible.
    let delivery = scheduler::with(|s| {
        let cur = s.current;
        let t = &s.tasks[cur];
        if t.ring != Ring::User || t.signals.saved.is_some() {
            return None;
        }
        t.signals
            .next_deliverable()
            .map(|(sig, h)| (sig, h, t.signals.restorer))
    });

    let (sig, handler, restorer) = match delivery {
        Some(v) => v,
        None => return,
    };

    if restorer == 0 {
        // No trampoline registered: we cannot safely return from the handler.
        // Leave the signal pending rather than risk a crash.
        return;
    }

    // The CPU iret frame sits immediately above the SyscallFrame on the kernel
    // stack (see do_fork/do_exec for the same layout): index 15=RIP, 16=CS,
    // 17=RFLAGS, 18=RSP, 19=SS.
    let frame_base = frame as *mut crate::syscall::SyscallFrame as *mut u64;
    let (orig_rip, orig_rflags, orig_rsp) =
        unsafe { (*frame_base.add(15), *frame_base.add(17), *frame_base.add(18)) };

    // Build the handler's user stack: push the restorer trampoline as the
    // return address. Align so that on handler entry rsp % 16 == 8 (the state
    // right after a `call`, as the System V ABI requires).
    let mut new_rsp = orig_rsp & !0xF;
    new_rsp -= 8;
    unsafe {
        core::ptr::write(new_rsp as *mut u64, restorer);
    }

    // Save the interrupted context for SYS_SIGRETURN.
    scheduler::with(|s| {
        let cur = s.current;
        s.tasks[cur].signals.saved = Some(SavedSigContext {
            rip: orig_rip,
            rflags: orig_rflags,
            rsp: orig_rsp,
            rax: syscall_ret,
        });
        s.tasks[cur].signals.clear_pending(sig);
    });

    // Rewrite the trap frame to enter the handler: RIP=handler, RSP=new_rsp,
    // and pass the signal number in rdi (first System V argument).
    unsafe {
        *frame_base.add(15) = handler; // RIP -> handler
        *frame_base.add(18) = new_rsp; // RSP -> crafted stack
    }
    frame.rdi = sig as u64;

    serial_println!("[signal] delivering signal {} to task {}", sig, current_id());
}

/// Implement `SYS_SIGRETURN`: restore the context saved by
/// [`deliver_pending_signals`] when a user handler returns via its trampoline.
///
/// Returns the value the originally-interrupted syscall should yield in `rax`
/// (the dispatcher stores this into the frame's rax slot, so the resumed code
/// observes the correct result). If no handler is in flight this is a no-op
/// that returns `u64::MAX`.
pub fn do_sigreturn(frame: &mut crate::syscall::SyscallFrame) -> u64 {
    let saved = scheduler::with(|s| {
        let cur = s.current;
        s.tasks[cur].signals.saved.take()
    });

    let ctx = match saved {
        Some(c) => c,
        None => return u64::MAX,
    };

    let frame_base = frame as *mut crate::syscall::SyscallFrame as *mut u64;
    unsafe {
        *frame_base.add(15) = ctx.rip; // restore RIP
        *frame_base.add(17) = ctx.rflags; // restore RFLAGS
        *frame_base.add(18) = ctx.rsp; // restore RSP
    }

    // The resumed syscall observes its original return value in rax.
    ctx.rax
}

/// Reap any orphaned zombie children that were reparented to init.
///
/// Called periodically by [`init_reaper_task`]. This is what makes init the
/// "reaper of last resort": processes whose parent exited before them are
/// adopted by init in [`do_exit`], and their zombie slots are cleaned up here.
pub fn reap_orphans() {
    let reaper = match init_task_id() {
        Some(r) => r,
        None => return,
    };

    // Snapshot init's children, then find which are zombies.
    let children: Vec<u64> = scheduler::with(|s| {
        s.tasks
            .iter()
            .find(|t| t.id == reaper)
            .map(|t| t.children.clone())
            .unwrap_or_default()
    });
    if children.is_empty() {
        return;
    }

    let zombies: Vec<u64> = scheduler::with(|s| {
        children
            .iter()
            .copied()
            .filter(|&cid| s.tasks.iter().any(|t| t.id == cid && t.state == State::Zombie))
            .collect()
    });

    for cid in zombies {
        scheduler::with(|s| {
            if let Some(init_t) = s.tasks.iter_mut().find(|t| t.id == reaper) {
                init_t.children.retain(|&c| c != cid);
            }
            if let Some(ct) = s.tasks.iter_mut().find(|t| t.id == cid) {
                ct.state = State::Dead;
            }
        });
        crate::fs::cleanup_task_fds(cid as u32);
        serial_println!("[init] reaped orphan task {}", cid);
    }
}

/// The init / reaper task (conventionally PID 1). Periodically reaps orphaned
/// zombie children so terminated, parentless processes do not leak forever.
pub extern "C" fn init_reaper_task() -> ! {
    serial_println!("[init] reaper task online (pid {})", current_id());
    loop {
        reap_orphans();
        sleep_yield();
    }
}

/// Cooperatively give up the CPU. The timer also preempts, but explicit yields
/// keep idle/looping tasks friendly and make the demo output readable.
pub fn yield_now() {
    x86_64::instructions::interrupts::without_interrupts(scheduler::schedule);
}

/// Alias used by demo/looping tasks for readability.
pub fn sleep_yield() {
    yield_now();
}

/// Sleep for a specified number of ticks.
/// For now, this is a simple busy-wait implementation.
/// In a real system, we'd block the task and wake it after the tick count.
pub fn sleep_ticks(ticks: u64) {
    if ticks == 0 {
        return;
    }
    let start = scheduler::ticks();
    while scheduler::ticks() < start + ticks {
        yield_now();
    }
}

/// The id of the currently running task.
pub fn current_id() -> u64 {
    scheduler::with(|s| s.tasks[s.current].id)
}

/// The id of the currently running task, read from a lock-free atomic.
///
/// Unlike [`current_id`] this never touches the scheduler spinlock, so it is
/// safe to call from `print!`/capture paths that may run with other locks held.
/// The value is updated on every context switch (see [`scheduler::CURRENT_TASK_ID`]).
pub fn current_id_fast() -> u64 {
    scheduler::CURRENT_TASK_ID.load(Ordering::Relaxed)
}

/// Block the current task (until [`unblock`]) and switch to another task.
pub fn block_current_and_schedule() {
    x86_64::instructions::interrupts::disable();
    scheduler::with(|s| {
        let cur = s.current;
        s.tasks[cur].state = State::Blocked;
    });
    scheduler::schedule();
    x86_64::instructions::interrupts::enable();
}

/// Mark a blocked task runnable again (used by IPC when a message arrives).
pub fn unblock(id: u64) {
    scheduler::with(|s| {
        if let Some(t) = s.tasks.iter_mut().find(|t| t.id == id) {
            if t.state == State::Blocked {
                t.state = State::Runnable;
            }
        }
    });
}

/// The lowest-priority task: when nothing else is runnable, halt until the next
/// interrupt, then yield to give woken tasks a chance to run.
pub extern "C" fn idle_task() -> ! {
    loop {
        x86_64::instructions::hlt();
        yield_now();
    }
}

/// The round-robin scheduler and its global state.
pub mod scheduler {
    use super::*;
    use x86_64::PhysAddr;
    use core::sync::atomic::AtomicU64;

    /// Number of timer ticks observed since boot (exposed for uptime/metrics).
    pub static TICKS: AtomicU64 = AtomicU64::new(0);

    /// Id of the task currently on-CPU, mirrored into a lock-free atomic so hot
    /// paths (notably `vga_buffer::_print` and the output-capture machinery) can
    /// learn who is running without taking the scheduler spinlock — taking it
    /// there would risk a re-entrant deadlock if a print ever happened while the
    /// scheduler lock was held. It is updated on every context switch.
    pub static CURRENT_TASK_ID: AtomicU64 = AtomicU64::new(0);
    
    /// The kernel's original CR3 (saved at init time for restoring when switching to kernel tasks).
    static KERNEL_CR3: Mutex<Option<PhysAddr>> = Mutex::new(None);
    
    /// Get the kernel's original CR3 (for creating clean user page tables).
    pub fn kernel_cr3() -> Option<PhysAddr> {
        *KERNEL_CR3.lock()
    }

    /// Global scheduler state: the task list and the index of the running task.
    pub struct Scheduler {
        pub tasks: Vec<Box<Task>>,
        pub current: usize,
    }

    static SCHEDULER: Mutex<Option<Scheduler>> = Mutex::new(None);

    /// Initialize the (empty) scheduler. Call once after the heap is up.
    pub fn init() {
        *SCHEDULER.lock() = Some(Scheduler {
            tasks: Vec::new(),
            current: 0,
        });
        
        // Save the kernel CR3 for potential future use
        use x86_64::registers::control::Cr3;
        let (frame, _) = Cr3::read();
        *KERNEL_CR3.lock() = Some(frame.start_address());
    }

    /// Run a closure with mutable access to scheduler state.
    ///
    /// We hold the lock with interrupts disabled so the timer ISR can never
    /// fire (and re-enter the scheduler) while the lock is held, and so a task
    /// can never be preempted mid-critical-section — either of which would
    /// deadlock a single-CPU spinlock. Nesting is fine: the previous interrupt
    /// state is saved/restored, so callers that already disabled interrupts
    /// (e.g. `schedule`) stay disabled.
    pub fn with<R>(f: impl FnOnce(&mut Scheduler) -> R) -> R {
        x86_64::instructions::interrupts::without_interrupts(|| {
            let mut guard = SCHEDULER.lock();
            let s = guard.as_mut().expect("scheduler not initialized");
            f(s)
        })
    }

    /// Total number of timer ticks since boot.
    pub fn ticks() -> u64 {
        TICKS.load(Ordering::Relaxed)
    }

    /// Snapshot the task table for diagnostics (the shell `ps` command).
    pub fn list() -> Vec<(u64, String, State)> {
        with(|s| {
            s.tasks
                .iter()
                .map(|t| (t.id, t.name.clone(), t.state))
                .collect()
        })
    }

    /// Called from the timer ISR (interrupts already disabled). Counts the tick
    /// and preempts the current task.
    pub fn on_timer_tick() {
        TICKS.fetch_add(1, Ordering::Relaxed);
        // Preempt: switch to the next runnable task, if any.
        schedule();
    }

    /// Core scheduling decision + context switch.
    ///
    /// Preconditions: interrupts are disabled. Picks the next `Runnable` task
    /// round-robin and switches to it. Returns to the caller only when *this*
    /// task is scheduled again later.
    pub fn schedule() {
        // Decide the next task and gather the raw pointers needed for the
        // switch, then drop the lock *before* switching (holding a spinlock
        // across a context switch would deadlock the next task).
        let switch: Option<(*mut u64, u64, VirtAddr, Option<PhysAddr>)> = with(|s| {
            let n = s.tasks.len();
            if n == 0 {
                return None;
            }
            let cur = s.current;

            // Find the next runnable task after `cur` (round-robin).
            let mut idx = cur;
            let mut found = None;
            for _ in 0..n {
                idx = (idx + 1) % n;
                if s.tasks[idx].state == State::Runnable {
                    found = Some(idx);
                    break;
                }
            }

            let next = match found {
                Some(i) => i,
                None => {
                    // Nobody else is runnable. Keep running current if it can.
                    if s.tasks[cur].state == State::Running
                        || s.tasks[cur].state == State::Runnable
                    {
                        s.tasks[cur].state = State::Running;
                        return None; // no switch needed
                    }
                    // Current is blocked/dead and nothing is runnable: this
                    // should not happen because `idle` is always runnable.
                    return None;
                }
            };

            if next == cur {
                // Only the current task is runnable; nothing to switch to.
                if s.tasks[cur].state != State::Dead {
                    s.tasks[cur].state = State::Running;
                }
                return None;
            }

            // Demote the outgoing task (unless it blocked/died) and promote the
            // incoming one.
            if s.tasks[cur].state == State::Running {
                s.tasks[cur].state = State::Runnable;
            }
            s.tasks[next].state = State::Running;
            s.current = next;
            // Mirror the incoming task's id into the lock-free atomic for
            // print/capture routing (see `CURRENT_TASK_ID`).
            CURRENT_TASK_ID.store(s.tasks[next].id, Ordering::Relaxed);

            let old_ptr = &mut s.tasks[cur].rsp as *mut u64;
            let new_rsp = s.tasks[next].rsp;
            let new_top = s.tasks[next].kstack_top;
            // The address space the incoming task must run in. User tasks each
            // have their own CR3; kernel tasks run in the shared kernel CR3.
            // We MUST reload this on every switch: `context_switch` only swaps
            // the kernel stack, and the user trampoline only loads CR3 on a
            // task's *first* run. Resuming an already-running user task would
            // otherwise keep whatever CR3 was active (often another task's),
            // executing it in the wrong address space — an intermittent, timing
            // dependent source of corruption and page faults.
            let next_cr3 = if s.tasks[next].ring == Ring::User {
                s.tasks[next].user_cr3
            } else {
                *KERNEL_CR3.lock()
            };
            Some((old_ptr, new_rsp, new_top, next_cr3))
        });

        if let Some((old_ptr, new_rsp, new_top, next_cr3)) = switch {
            // The CPU must use the incoming task's kernel stack for any future
            // interrupt/syscall taken while it runs.
            gdt::set_kernel_stack(new_top);

            // Switch address space to the incoming task's. Skip the (expensive,
            // TLB-flushing) reload if we're already in the right CR3. All kernel
            // mappings are shared across every CR3, so the kernel code executing
            // here continues uninterrupted after the swap.
            if let Some(cr3) = next_cr3 {
                use x86_64::registers::control::Cr3;
                let (cur_frame, _) = Cr3::read();
                if cur_frame.start_address() != cr3 {
                    unsafe {
                        core::arch::asm!(
                            "mov cr3, {}",
                            in(reg) cr3.as_u64(),
                            options(nostack, preserves_flags)
                        );
                    }
                }
            }

            // SAFETY: pointers come from stable heap-boxed tasks; interrupts
            // are disabled by the caller.
            unsafe { context_switch(old_ptr, new_rsp) };
        }
    }

    /// Start scheduling: switch into the first runnable task. Never returns —
    /// the boot context is abandoned (its registers are saved into a discard
    /// slot we never resume).
    pub fn run() -> ! {
        x86_64::instructions::interrupts::disable();

        let (first_rsp, first_top) = with(|s| {
            assert!(!s.tasks.is_empty(), "no tasks to run");
            // Pick the first runnable task.
            let idx = s
                .tasks
                .iter()
                .position(|t| t.state == State::Runnable)
                .expect("no runnable task");
            s.tasks[idx].state = State::Running;
            s.current = idx;
            CURRENT_TASK_ID.store(s.tasks[idx].id, Ordering::Relaxed);
            (s.tasks[idx].rsp, s.tasks[idx].kstack_top)
        });

        gdt::set_kernel_stack(first_top);

        // We never come back to the boot context; give context_switch a scratch
        // location to "save" the (discarded) boot registers into.
        let mut discard: u64 = 0;
        unsafe { context_switch(&mut discard as *mut u64, first_rsp) };

        unreachable!("scheduler::run returned to boot context");
    }
}
