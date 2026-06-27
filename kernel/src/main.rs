//! # PICKLE OS Kernel
//!
//! A memory-safe x86_64 **microkernel** written in Rust. Unlike a monolithic
//! kernel, PICKLE OS keeps the privileged core small: scheduling, memory, IPC and
//! capability enforcement live in the kernel; everything else (drivers, file
//! systems, the network stack, the display server) is designed to run as
//! isolated user-space tasks that communicate through message-passing IPC.
//!
//! This file is the freestanding binary entry point. Because we run on bare
//! metal there is no standard library and no `main` provided by libc:
//!   * `#![no_std]`  — do not link the Rust standard library.
//!   * `#![no_main]` — do not use the normal Rust entry point; the bootloader
//!     calls our `kernel_main` (wired up by the `entry_point!` macro) directly.
//!
//! ## Boot flow
//! 1. The `bootloader` crate (BIOS) loads the kernel, switches the CPU into
//!    64-bit long mode, sets up an initial page table + stack, and gathers a
//!    [`BootInfo`] structure (memory map + physical-memory offset).
//! 2. It jumps to [`kernel_main`].
//! 3. [`kernel_main`] initializes every subsystem in order (see [`init`]),
//!    spawns the initial tasks, and hands control to the scheduler.

#![no_std]
#![no_main]
//
// --- Unstable features used by the kernel (require nightly Rust) ------------
//
// Special calling convention for interrupt handlers (push/pop all registers,
// `iretq` to return). Used throughout `interrupts.rs`.
#![feature(abi_x86_interrupt)]
// Custom handler invoked when a heap allocation fails (see `allocator.rs`).
#![feature(alloc_error_handler)]

// The kernel uses dynamic allocation (Box/Vec/String/BTreeMap) once the heap
// is initialized. `alloc` is the allocation half of the standard library and
// is available on `no_std` once a `#[global_allocator]` is provided.
extern crate alloc;

use bootloader_api::{entry_point, BootInfo, BootloaderConfig};
use bootloader_api::config::Mapping;
use core::panic::PanicInfo;

// --- Kernel subsystem modules ----------------------------------------------
mod allocator; // Kernel heap: global allocator + heap region mapping.
mod capability; // Capability table: unforgeable handles to kernel objects.
mod driver; // User-space-style device drivers (IRQ bridge, port-IO, keyboard, timer).
mod elf; // ELF binary loader for user-space processes.
mod fs; // NextFS: on-disk file system over block devices.
mod gdt; // Global Descriptor Table + TSS (privilege levels, fault stacks).
mod gui; // Compositing window manager: desktop, taskbar, draggable windows.
mod init_user; // User-space init: seeds NextFS and launches programs from disk.
mod interrupts; // IDT, CPU exceptions, PIC, timer + keyboard IRQs.
mod ipc; // Synchronous message-passing endpoints between tasks.
mod memory; // Paging (OffsetPageTable) + physical frame allocator.
mod net; // TCP/IP networking subsystem using smoltcp.
mod serial; // 16550 UART driver -> host terminal (logging/tests).
mod services; // Phase 2 core OS services: init, registry, vfs, memfs.
mod shell; // Interactive in-kernel shell.
mod signal; // POSIX-style signal numbers and per-task signal state.
mod syscall; // System-call ABI + dispatch (the kernel/user boundary).
mod terminal; // Text-grid terminal model behind the GUI Terminal window.
mod task; // Tasks, scheduler, and low-level context switching.
mod userprogs; // Embedded user-space ELF binaries.
mod vga_buffer; // VGA text-mode console (0xb8000).
mod framebuffer; // Linear pixel framebuffer graphics driver (bootloader 0.11).
mod wm; // Window-server core: window registry, shared buffers, event queues.

/// Bootloader configuration: request a dynamically-mapped physical memory
/// region (so the kernel can read/write any physical frame via an offset, just
/// like the old `map_physical_memory` feature) and a framebuffer of at least
/// 1024x768 so we have room for a graphical desktop.
const BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config.frame_buffer.minimum_framebuffer_width = Some(1024);
    config.frame_buffer.minimum_framebuffer_height = Some(768);
    config
};

// Tell the bootloader which function is our entry point + our boot config.
entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

/// The kernel's true entry point, called by the bootloader with hardware info.
///
/// `boot_info` contains:
///   * `memory_map` — regions of usable vs. reserved physical RAM, used to seed
///     the physical frame allocator.
///   * `physical_memory_offset` — the virtual address at which the bootloader
///     mapped all of physical memory (the `map_physical_memory` feature),
///     letting us read/write any physical frame by adding this offset.
fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    // Raw serial probe FIRST (port I/O needs no memory mapping), so we know the
    // kernel is executing even if VGA/paging is misbehaving.
    unsafe {
        let mut p = x86_64::instructions::port::Port::<u8>::new(0x3F8);
        for b in *b"PICKLE OS:_start reached\n" {
            p.write(b);
        }
    }
    serial_println!("PICKLE OS :: serial console online");

    // Take ownership of the framebuffer (if the bootloader provided one) and
    // initialise the graphics driver before anything else draws to the screen.
    if let Some(fb) = boot_info.framebuffer.as_mut() {
        let info = fb.info();
        framebuffer::init(fb.buffer_mut().as_mut_ptr(), info);
        serial_println!(
            "PICKLE OS :: framebuffer {}x{} {:?} {} bpp, stride {}",
            info.width, info.height, info.pixel_format, info.bytes_per_pixel, info.stride
        );
    } else {
        serial_println!("PICKLE OS :: WARNING no framebuffer provided by bootloader");
    }

    serial_println!("PICKLE OS :: booting microkernel v{}", env!("CARGO_PKG_VERSION"));

    // Draw an early boot test pattern to prove per-pixel graphics work before
    // the heavier subsystems come up.
    framebuffer::draw_boot_test_pattern();

    // Downgrade the exclusive boot-info reference now that we're done with the
    // framebuffer; the rest of init only needs read access.
    let boot_info: &'static BootInfo = boot_info;

    // Bring every subsystem up in dependency order.
    init::init_all(boot_info);

    println!("\nPICKLE OS :: kernel initialized. Starting scheduler + shell.\n");

    // Spawn the built-in demo/diagnostic tasks. These exercise the scheduler,
    // IPC and the capability system so you can see the OS actually working.
    task::spawn_kernel_task("idle", task::idle_task);
    // Spawn `init` (the reaper) as the very next task so it is assigned PID 1.
    // It adopts orphaned children and reaps any zombies that have no living
    // parent to wait() for them, mirroring the role of PID 1 on Unix systems.
    let init_id = task::spawn_kernel_task("init", task::init_reaper_task);
    task::set_init_task(init_id);
    task::spawn_kernel_task("heartbeat", demo::heartbeat_task);
    task::spawn_kernel_task("ipc-pong", demo::ipc_pong_task);
    task::spawn_kernel_task("ipc-ping", demo::ipc_ping_task);
    task::spawn_kernel_task("shell", shell::shell_task);

    // Bring up the Phase 2 core OS services (Init -> Registry -> VFS/MemFS).
    // These run as tasks and communicate purely over IPC.
    services::start();

    // Bring up the Phase 3 device drivers (keyboard, timer monitor). These are
    // ordinary tasks that receive hardware IRQs as notifications and touch
    // device ports through capability-checked accessors.
    driver::start();

    // Bring up the graphical desktop: a compositing window manager task that
    // owns the framebuffer, draws the desktop/taskbar/windows, and follows the
    // PS/2 mouse. The text shell keeps running underneath (its console output is
    // redirected to the serial port while the GUI owns the screen).
    task::spawn_kernel_task("compositor", gui::compositor_task);

    // Window-server bring-up: a one-shot self-test of the `wm` core (runs in a
    // real task context so ownership checks are meaningful), and a small demo
    // "client" that opens a window, animates a gradient, and logs the input
    // events the compositor routes to it — exercising the full SYS_WIN_* path.
    task::spawn_kernel_task("wm-selftest", gui::wm_selftest_task);
    task::spawn_kernel_task("win-demo", gui::client_demo_task);

    // Network demo task: continuously polls the network stack and provides
    // basic services (ICMP ping responder, TCP echo server on port 7).
    task::spawn_kernel_task("net-demo", net::demo::network_demo_task);

    // User-space programs are no longer hard-coded here. Once the AHCI/NextFS
    // bring-up task finishes mounting the filesystem, it hands control to
    // `init_user::run()`, which seeds `/bin` from the embedded images, writes
    // `/etc/inittab`, and launches each listed program straight from disk (ring
    // 3). See `driver::ahci_init_task` and `kernel/src/init_user.rs`.
    //
    // If NextFS is unavailable (fewer than two block devices), `init` simply
    // does not run and no user programs are launched.

    // Enable interrupts and hand the CPU to the scheduler. The timer interrupt
    // will preempt tasks; we never return from here.
    x86_64::instructions::interrupts::enable();
    task::scheduler::run()
}

/// Centralized, ordered subsystem initialization. Order matters: e.g. the GDT
/// must be loaded before the IDT references its TSS selectors, and paging must
/// be up before the heap can be mapped.
mod init {
    use super::*;
    use x86_64::VirtAddr;

    pub fn init_all(boot_info: &'static BootInfo) {
        // 1. CPU structures: segment descriptors + task-state segment (which
        //    provides a known-good stack for double faults).
        gdt::init();
        // 2. Interrupt descriptor table + remap & unmask the PIC, so timer and
        //    keyboard IRQs are delivered to our handlers.
        interrupts::init_idt();
        unsafe { interrupts::PICS.lock().initialize() };

        // 3. Virtual memory. Build an `OffsetPageTable` over the bootloader's
        //    page tables and a frame allocator seeded from the memory regions.
        //    With bootloader 0.11 + `Mapping::Dynamic`, the physical memory is
        //    mapped at a bootloader-chosen offset reported in the boot info.
        let phys_offset = boot_info
            .physical_memory_offset
            .into_option()
            .expect("bootloader did not map physical memory (config mismatch)");
        let phys_mem_offset = VirtAddr::new(phys_offset);
        let mut mapper = unsafe { memory::init(phys_mem_offset) };
        let mut frame_allocator =
            unsafe { memory::BootInfoFrameAllocator::init(&boot_info.memory_regions) };

        // 4. Kernel heap. Map the heap region and hand it to the global
        //    allocator so `Box`/`Vec`/`String` become usable everywhere after.
        allocator::init_heap(&mut mapper, &mut frame_allocator)
            .expect("heap initialization failed");

        // 5. Stash the mapper + frame allocator globally so later subsystems
        //    (task stacks, user processes) can allocate and map memory.
        memory::install_global(mapper, frame_allocator, phys_mem_offset);

        // 6. Subsystems that need the heap: scheduler, IPC registry, caps.
        task::scheduler::init();
        ipc::init();
        capability::init();
        // Window-server core: window registry + shared buffers + event queues.
        // Needs the heap; the compositor (gui) and the SYS_WIN_* syscalls build
        // on top of it.
        wm::init();

        // 7. Driver subsystems: DMA pool + PCI bus enumeration.
        //    AHCI init is deferred to a task (after the scheduler starts) because
        //    it needs to mint capabilities, which requires a valid task context.
        driver::dma::init();
        driver::pci::init();

        // 8. Network subsystem: TCP/IP stack using smoltcp.
        net::init();

        println!("init :: all subsystems online");
    }
}

/// Built-in demonstration tasks that prove the OS is actually multitasking and
/// doing IPC. These are ordinary kernel tasks; replace/extend them freely.
mod demo {
    use crate::ipc;
    use crate::task;

    /// Prints a heartbeat periodically so you can see preemptive scheduling.
    pub extern "C" fn heartbeat_task() -> ! {
        let mut n: u64 = 0;
        loop {
            if n % 5 == 0 {
                crate::serial_println!("[heartbeat] tick {}", n);
            }
            n += 1;
            task::sleep_yield(); // cooperatively yield; timer also preempts.
        }
    }

    /// IPC demo: receives "ping" messages on a well-known endpoint and replies.
    pub extern "C" fn ipc_pong_task() -> ! {
        let ep = ipc::create_named_endpoint("demo.pong");
        loop {
            let msg = ipc::receive(ep);
            crate::serial_println!("[pong] got {} from task {}", msg.tag, msg.sender);
            ipc::reply(&msg, ipc::Message::new(msg.tag + 1));
        }
    }

    /// IPC demo: sends a "ping" and waits for the "pong" reply, then sleeps.
    pub extern "C" fn ipc_ping_task() -> ! {
        // Give pong a chance to register its endpoint.
        for _ in 0..3 {
            task::sleep_yield();
        }
        let ep = ipc::lookup("demo.pong").expect("pong endpoint not found");
        let mut tag = 100;
        loop {
            let reply = ipc::call(ep, ipc::Message::new(tag));
            crate::serial_println!("[ping] sent {}, got reply {}", tag, reply.tag);
            tag += 100;
            for _ in 0..50 {
                task::sleep_yield();
            }
        }
    }
}

/// Panic handler for normal (non-test) builds. On bare metal there is nowhere
/// to unwind to, so we log the panic to both consoles and halt the CPU.
#[cfg(not(test))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("\n*** KERNEL PANIC ***\n{}", info);
    serial_println!("\n*** KERNEL PANIC ***\n{}", info);
    print_backtrace();
    hlt_loop();
}

/// Park the CPU efficiently: `hlt` sleeps until the next interrupt instead of
/// busy-spinning, which keeps host CPU usage low under QEMU.
pub fn hlt_loop() -> ! {
    loop {
        x86_64::instructions::hlt();
    }
}

/// Print a stack backtrace by walking the RBP-linked list.
///
/// In x86_64 System V ABI, `rbp` points to a saved `rbp` at `[rbp + 0]` and
/// the return address is at `[rbp + 8]`. A sentinel frame has `rbp = 0`.
/// If rbp is not chained (compiler omitted frame pointers), this prints nothing.
pub fn print_backtrace() {
    serial_println!("\n[backtrace]");
    // Read rbp via inline asm.
    let rbp: u64;
    unsafe {
        core::arch::asm!("mov {}, rbp", out(reg) rbp);
    }
    let mut fp = rbp;
    let mut i = 0usize;
    while fp != 0 && i < 32 {
        // Read saved frame pointer and return address from the stack.
        // Safety: these are kernel-owned stack frames; if the kernel has
        // corrupted its own stack this can fault — but we're already panicking.
        let saved_fp: u64;
        let ret_addr: u64;
        unsafe {
            saved_fp = core::ptr::read_volatile(fp as *const u64);
            ret_addr = core::ptr::read_volatile((fp + 8) as *const u64);
        }
        if ret_addr == 0 {
            break;
        }
        serial_println!("  #{}: {:#018x}", i, ret_addr);
        fp = saved_fp;
        i += 1;
    }
}
