//! Interactive in-kernel shell.
//!
//! This is a tiny but real command interpreter that runs as an ordinary kernel
//! task. It reads keystrokes that the keyboard interrupt handler pushes into a
//! lock-free-ish ring buffer, decodes scancodes into characters, and executes
//! built-in commands. It demonstrates the whole stack working together:
//! interrupts -> input queue -> scheduling -> syscalls -> IPC -> capabilities.
//!
//! When PICKLE OS gains a user-space init system, this shell becomes a user
//! program talking to the kernel purely through syscalls/IPC; for now it lives
//! in the kernel so you have something interactive on first boot.

use crate::driver::console;
use crate::driver::console::keys;
use crate::syscall;
use crate::task;
use crate::{print, println, serial_println};
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

/// Maximum number of past command lines kept for up/down history recall.
const HISTORY_LIMIT: usize = 64;

/// NextFS path where command history is persisted across reboots.
const HISTORY_FILE: &str = "/.pickleos_history";

/// Set once the persisted history has been loaded from disk, so only the first
/// shell task performs the load (later shells share the in-memory `HISTORY`).
static HISTORY_LOADED: AtomicBool = AtomicBool::new(false);

/// Global command history, shared between the interactive line editor (which
/// appends to it and walks it for up/down recall) and the `history` builtin
/// (which lists it, and can therefore be piped, e.g. `history | grep ps`).
/// A `spin::Mutex` keeps it safe even though today only the shell task touches
/// it.
static HISTORY: spin::Mutex<Vec<String>> = spin::Mutex::new(Vec::new());

/// Append a command line to the global history, de-duplicating consecutive
/// repeats and capping the total at [`HISTORY_LIMIT`]. Persists the updated
/// history to disk on change.
fn history_push(line: &str) {
    let mut changed = false;
    {
        let mut h = HISTORY.lock();
        if h.last().map(|s| s.as_str()) != Some(line) {
            h.push(line.into());
            if h.len() > HISTORY_LIMIT {
                h.remove(0);
            }
            changed = true;
        }
    }
    if changed {
        history_save();
    }
}

/// Snapshot the current history (oldest first).
fn history_snapshot() -> Vec<String> {
    HISTORY.lock().clone()
}

/// Persist the in-memory history to [`HISTORY_FILE`] on NextFS (one line per
/// entry). Wrapped in a [`KernelIoGuard`] so the trusted in-kernel shell can
/// drive the disk even though it holds no MMIO capability of its own. Errors
/// (e.g. no filesystem mounted) are silently ignored — history is best-effort.
fn history_save() {
    let snapshot = history_snapshot();
    let mut data = String::new();
    for entry in &snapshot {
        data.push_str(entry);
        data.push('\n');
    }
    let _io = crate::driver::mmio::KernelIoGuard::enter();
    let _ = nextfs_write_file(HISTORY_FILE, data.as_bytes());
}

/// Load persisted history from [`HISTORY_FILE`] into the in-memory `HISTORY`.
/// Runs at most once (guarded by [`HISTORY_LOADED`]); later shells reuse the
/// already-populated history. Must be called only after the filesystem is
/// mounted. Reports the outcome over serial.
fn history_load() {
    if HISTORY_LOADED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _io = crate::driver::mmio::KernelIoGuard::enter();
    match nextfs_read_file(HISTORY_FILE) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            let mut h = HISTORY.lock();
            for line in text.lines() {
                let t = line.trim();
                if t.is_empty() {
                    continue;
                }
                if h.last().map(|s| s.as_str()) == Some(t) {
                    continue;
                }
                h.push(t.into());
                if h.len() > HISTORY_LIMIT {
                    h.remove(0);
                }
            }
            let n = h.len();
            drop(h);
            serial_println!("[history] loaded {} entries from {}", n, HISTORY_FILE);
        }
        Err(_) => {
            serial_println!("[history] no saved history at {} (fresh start)", HISTORY_FILE);
        }
    }
}

/// An in-place line editor backing the interactive prompt.
///
/// It maintains the current input as a `Vec<char>` plus a cursor position
/// (a char index, not a byte index) so editing keys can insert/delete anywhere
/// in the line. All visual updates go through [`print!`], which the GUI routes
/// into the on-screen Terminal model (and which also reaches the serial console
/// on text-only builds), so the editor never touches the terminal grid
/// directly. A single redraw primitive repaints the line after every edit,
/// which keeps the logic simple and correct for the short commands a shell
/// prompt sees (the line is assumed to fit on one terminal row).
struct LineEditor {
    /// The prompt string redrawn at column 0 on every repaint.
    prompt: &'static str,
    /// Current line contents.
    chars: Vec<char>,
    /// Cursor position as an index into `chars` (0..=chars.len()).
    cursor: usize,
    /// Number of glyphs painted after the prompt on the previous redraw, so we
    /// know how many trailing cells to blank when the line shrinks.
    painted: usize,
    /// A snapshot of the global history taken when up/down navigation begins,
    /// so the indices stay stable while browsing.
    hist: Vec<String>,
    /// Navigation index into `hist`. `None` means "editing a fresh line".
    hist_pos: Option<usize>,
    /// The line being edited before history navigation began, so Down can
    /// restore it after browsing upward.
    draft: Vec<char>,
}

impl LineEditor {
    fn new(prompt: &'static str) -> Self {
        Self {
            prompt,
            chars: Vec::new(),
            cursor: 0,
            painted: 0,
            hist: Vec::new(),
            hist_pos: None,
            draft: Vec::new(),
        }
    }

    /// Repaint the prompt + line, blank any leftover trailing glyphs from a
    /// longer previous line, then place the cursor at `self.cursor`.
    fn redraw(&mut self) {
        // Return to column 0 and reprint the prompt and full line.
        print!("\r{}", self.prompt);
        for &c in &self.chars {
            print!("{}", c);
        }
        // Blank any cells left over from a previously longer line.
        let extra = self.painted.saturating_sub(self.chars.len());
        for _ in 0..extra {
            print!(" ");
        }
        // Move back over the blanks we just printed.
        for _ in 0..extra {
            print!("\u{8}");
        }
        // Now the cursor sits at end-of-line; walk it left to `self.cursor`.
        for _ in self.cursor..self.chars.len() {
            print!("\u{8}");
        }
        self.painted = self.chars.len();
    }

    fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
        self.redraw();
    }

    /// Backspace: delete the char before the cursor.
    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
            self.redraw();
        }
    }

    /// Forward-delete: delete the char under the cursor.
    fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
            self.redraw();
        }
    }

    fn left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            print!("\u{8}");
        }
    }

    fn right(&mut self) {
        if self.cursor < self.chars.len() {
            print!("{}", self.chars[self.cursor]);
            self.cursor += 1;
        }
    }

    fn home(&mut self) {
        while self.cursor > 0 {
            self.cursor -= 1;
            print!("\u{8}");
        }
    }

    fn end(&mut self) {
        while self.cursor < self.chars.len() {
            print!("{}", self.chars[self.cursor]);
            self.cursor += 1;
        }
    }

    /// Replace the whole line with `new` and put the cursor at the end.
    fn set_line(&mut self, new: &[char]) {
        self.chars.clear();
        self.chars.extend_from_slice(new);
        self.cursor = self.chars.len();
        self.redraw();
    }

    /// Recall the previous (older) history entry.
    fn history_prev(&mut self) {
        match self.hist_pos {
            None => {
                // Start browsing: snapshot history + save the in-progress draft.
                self.hist = history_snapshot();
                if self.hist.is_empty() {
                    return;
                }
                self.draft = self.chars.clone();
                let idx = self.hist.len() - 1;
                self.hist_pos = Some(idx);
                let entry: Vec<char> = self.hist[idx].chars().collect();
                self.set_line(&entry);
            }
            Some(0) => {} // already at the oldest entry
            Some(idx) => {
                let idx = idx - 1;
                self.hist_pos = Some(idx);
                let entry: Vec<char> = self.hist[idx].chars().collect();
                self.set_line(&entry);
            }
        }
    }

    /// Recall the next (newer) history entry, or restore the draft past the end.
    fn history_next(&mut self) {
        match self.hist_pos {
            None => {}
            Some(idx) if idx + 1 < self.hist.len() => {
                let idx = idx + 1;
                self.hist_pos = Some(idx);
                let entry: Vec<char> = self.hist[idx].chars().collect();
                self.set_line(&entry);
            }
            Some(_) => {
                // Past the newest entry: restore the saved draft.
                self.hist_pos = None;
                let draft = self.draft.clone();
                self.set_line(&draft);
            }
        }
    }

    /// Commit the current line: return it as a `String`, record it in the global
    /// history, and reset the editor for the next prompt.
    fn take(&mut self) -> String {
        let line: String = self.chars.iter().collect();
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            history_push(trimmed);
        }
        self.chars.clear();
        self.cursor = 0;
        self.painted = 0;
        self.hist_pos = None;
        self.draft.clear();
        line
    }
}

/// The shell task entry point.
pub extern "C" fn shell_task() -> ! {
    let me = task::current_id();

    // Adopt a terminal. The compositor enqueues one terminal id per Terminal
    // window it opens; each shell task claims one here and binds itself as the
    // owner so its output is routed to that window and it can tell when the
    // window is closed. Wait for an id to appear (the first shell is spawned
    // before the compositor has created the primary terminal).
    let my_term = loop {
        if let Some(id) = crate::terminal::take_pending_shell() {
            break id;
        }
        task::sleep_ticks(1);
    };
    crate::terminal::bind(my_term, me);

    // Give the shell task its own capability table and a sample capability, so
    // the `caps` command has something real to show.
    crate::capability::create_table(me);
    if let Some(ep) = crate::ipc::lookup("demo.pong") {
        crate::capability::mint(
            me,
            crate::capability::Object::Endpoint(ep),
            crate::capability::Rights::SEND
                .union(crate::capability::Rights::RECV)
                .union(crate::capability::Rights::GRANT),
        );
    }

    // Wait briefly for the GUI compositor to take over the screen and route
    // console output into the on-screen Terminal window, so the banner/prompt
    // land there rather than only on the serial log. Falls through after a short
    // grace period on text-only builds where no compositor ever activates.
    for _ in 0..200 {
        if crate::terminal::is_active() {
            break;
        }
        task::sleep_ticks(1);
    }

    // Show the banner and prompt immediately. Persisted command history is
    // loaded lazily in the main loop below (see `history_load`) so a slow
    // first-boot filesystem format never delays the visible UI.
    print_banner();
    print_prompt();

    let mut editor = LineEditor::new(PROMPT);
    loop {
        // Lazily load persisted command history once the filesystem is actually
        // mounted. Only the first shell performs the load; the rest share the
        // in-memory copy. `fs::is_mounted` is non-blocking, so this never stalls
        // the input loop while the disk is still being formatted on first boot.
        if !HISTORY_LOADED.load(Ordering::SeqCst) && crate::fs::is_mounted() {
            history_load();
        }
        // If our window was closed, our terminal slot is freed (and possibly
        // handed to a new shell). Stop touching it and park indefinitely.
        if !crate::terminal::owns(my_term, me) {
            task::sleep_ticks(5);
            continue;
        }

        // Only the shell that owns the *focused* terminal consumes keystrokes;
        // others idle so input goes to whichever window the user clicked.
        if !crate::terminal::is_focused(my_term) {
            // Sleep briefly instead of busy-yielding so an unfocused shell
            // does not spin the scheduler and waste CPU.
            task::sleep_ticks(2);
            continue;
        }

        // Drain decoded characters from the shared console line discipline.
        while let Some(c) = console::read_char() {
            match c {
                '\n' | '\r' => {
                    println!();
                    let line = editor.take();
                    run_command_line(line.trim());
                    print_prompt();
                }
                // Backspace (ASCII BS or DEL when sent as erase). Forward-delete
                // arrives as the dedicated DELETE sentinel below.
                '\u{8}' => editor.backspace(),
                keys::DELETE => editor.delete(),
                keys::ARROW_LEFT => editor.left(),
                keys::ARROW_RIGHT => editor.right(),
                keys::ARROW_UP => editor.history_prev(),
                keys::ARROW_DOWN => editor.history_next(),
                keys::HOME => editor.home(),
                keys::END => editor.end(),
                // Page keys scroll this terminal's scrollback buffer rather than
                // editing the line.
                keys::PAGE_UP => {
                    crate::terminal::scroll_view(my_term, (crate::terminal::ROWS as i32) / 2)
                }
                keys::PAGE_DOWN => {
                    crate::terminal::scroll_view(my_term, -((crate::terminal::ROWS as i32) / 2))
                }
                // Ignore any other control characters; insert printable text.
                c if (c as u32) >= 0x20 => editor.insert(c),
                _ => {}
            }
        }
        // Nothing to do right now — let other tasks run.
        task::yield_now();
    }
}

fn print_banner() {
    println!("+------------------------------------------+");
    println!("|  PICKLE OS shell                         |");
    println!("|  type 'help' for the list of commands    |");
    println!("+------------------------------------------+");
}

/// The shell prompt string. Shared by the initial print and the line editor's
/// redraw so the two always agree on column 0's contents.
const PROMPT: &str = "pickleos> ";

fn print_prompt() {
    print!("{}", PROMPT);
}

/// Parse and run a full command line, honouring pipe (`|`) and output
/// redirection (`>`) operators before dispatching to [`execute`].
///
/// Pipelines connect commands by capturing the textual output of each stage and
/// feeding it as the standard input of the next (the shell's built-ins are
/// in-kernel functions rather than separate processes, so data is shuttled via
/// an in-memory capture buffer rather than a real fd — the kernel pipe buffer is
/// exercised end-to-end by the user-space `pipe_test` program instead).
/// Redirection writes the final stage's captured output to a NextFS file.
fn run_command_line(line: &str) {
    if line.is_empty() {
        return;
    }

    // Split off an optional output redirection: `... > file`.
    let (pipeline, redirect) = match line.split_once('>') {
        Some((l, r)) => (l.trim(), Some(r.trim())),
        None => (line, None),
    };

    if let Some(target) = redirect {
        if target.is_empty() {
            println!("syntax error: expected filename after '>'");
            return;
        }
    }

    // Split the pipeline into stages on '|'.
    let stages: alloc::vec::Vec<&str> = pipeline.split('|').map(|s| s.trim()).collect();
    if stages.iter().any(|s| s.is_empty()) {
        println!("syntax error: empty command in pipeline");
        return;
    }

    let mut input: Option<String> = None;
    let last = stages.len() - 1;
    for (i, stage) in stages.iter().enumerate() {
        // Capture a stage's output if it feeds another stage, or if the whole
        // pipeline's output is being redirected to a file.
        let capture = i != last || redirect.is_some();
        if capture {
            crate::vga_buffer::capture_begin();
        }
        execute(stage, input.as_deref());
        input = if capture {
            crate::vga_buffer::capture_end()
        } else {
            None
        };
    }

    // If redirecting, write the final captured output to the target file.
    if let Some(target) = redirect {
        let data = input.unwrap_or_default();
        match nextfs_write_file(target, data.as_bytes()) {
            Ok(()) => println!("[{} bytes written to {}]", data.len(), target),
            Err(e) => println!("redirect: {}: {}", target, e),
        }
    }
}

/// Execute one command. `input` carries piped standard input from a previous
/// pipeline stage (if any); most commands ignore it, while filters such as
/// `wc` and `grep` consume it.
fn execute(cmd: &str, input: Option<&str>) {
    let mut parts = cmd.split_whitespace();
    let verb = match parts.next() {
        Some(v) => v,
        None => return, // empty line
    };
    let rest = cmd[verb.len()..].trim();

    match verb {
        "help" => {
            println!("Built-in commands:");
            println!("  help          show this help");
            println!("  ps            list tasks and their states");
            println!("  pid           print this shell's task id (via syscall)");
            println!("  ticks         timer ticks since boot (via syscall)");
            println!("  uptime        approximate uptime in seconds");
            println!("  echo <text>   print text via the SYS_PRINT syscall");
            println!("  wc            count lines/words/bytes of piped input");
            println!("  grep <pat>    print piped-input lines matching <pat>");
            println!("  cmd1 | cmd2   pipe cmd1's output into cmd2 (e.g. ps | wc)");
            println!("  cmd > file    redirect cmd's output to a NextFS file");
            println!("  ipc <n>       RPC 'n' to the demo.pong service");
            println!("  caps          show this task's capabilities");
            println!("  mem           show heap configuration");
            println!("  services      list services registered with the registry");
            println!("  irqs          show IRQ line owners and delivered counts");
            println!("  dma           show DMA pool usage");
            println!("  pci           list PCI devices (also: lspci)");
            println!("  ahci          show AHCI SATA devices (also: disks)");
            println!("  identify      query all SATA drives (IDENTIFY DEVICE)");
            println!("  lsblk         list registered block devices (also: blkdev)");
            println!("  blkread <d> <lba>       hex-dump one block from device d");
            println!("  blkwrite <d> <lba> <s>  write string s into block at lba");
            println!("  mkfs.nextfs <dev>       format a block device with NextFS");
            println!("  mount <dev>             mount a NextFS from block device");
            println!("  unmount                   unmount the current filesystem");
            println!("  nxls [path]               list NextFS directory (default: root)");
            println!("  nxcat <path>              print NextFS file contents");
            println!("  nxwrite <path> <text>     write text to NextFS file");
            println!("  nxmkdir <path>            create NextFS directory");
            println!("  nxrm <path>               remove NextFS file");
            println!("  nxrmdir <path>            remove NextFS directory (must be empty)");
            println!("  nxtruncate <path> <size>  truncate NextFS file to size");
            println!("  nxstat <path>             show inode metadata (mode/owner/size)");
            println!("  nxchmod <octal> <path>    change permission bits");
            println!("  nxchown <uid> <gid> <path> change owner/group");
            println!("  ls [path]     list a directory (via VFS over IPC)");
            println!("  cat <path>    print a file's contents (via VFS)");
            println!("  write <p> <s> write string <s> to file <p> (via VFS)");
            println!("  touch <path>  create an empty file (via VFS)");
            println!("  mkdir <path>  create a directory (via VFS)");
            println!("  rm <path>     remove a file or empty dir (via VFS)");
            println!("  stat <path>   show metadata for a path (via VFS)");
            println!("  int3          trigger a breakpoint exception (safe)");
            println!("  clear         clear the screen");
            println!("  colors        show the ANSI colour palette");
            println!("  history       show recent command history (persists across reboots)");
            println!("line editing: arrows move/recall, Home/End jump, Del forward-deletes");
            println!("scrollback: PageUp/PageDown scroll this window's history");
        }
        "history" => {
            let h = history_snapshot();
            for (i, entry) in h.iter().enumerate() {
                println!("  {:>3}  {}", i + 1, entry);
            }
        }
        "ps" => {
            println!("  ID  STATE     NAME");
            for (id, name, state) in task::scheduler::list() {
                println!("  {:>2}  {:<8}  {}", id, alloc::format!("{:?}", state), name);
            }
        }
        "pid" => println!("shell task id = {}", syscall::sys_getpid()),
        "ticks" => println!("ticks = {}", syscall::sys_ticks()),
        "uptime" => {
            // The PIT is configured by the bootloader-default; ~18.2 Hz legacy
            // rate. We report ticks and a rough seconds estimate.
            let t = syscall::sys_ticks();
            println!("ticks = {} (~{} s at 18.2 Hz)", t, t / 18);
        }
        "echo" => {
            syscall::sys_print(rest);
            println!();
        }
        "wc" => {
            // Count lines/words/bytes of piped input (or the literal argument).
            let data = input.unwrap_or(rest);
            let lines = data.lines().count();
            let words = data.split_whitespace().count();
            let bytes = data.len();
            println!("  {:>6} {:>6} {:>6}", lines, words, bytes);
        }
        "grep" => {
            // Print piped-input lines containing the given pattern.
            if rest.is_empty() {
                println!("usage: <cmd> | grep <pattern>");
            } else {
                let data = input.unwrap_or("");
                for l in data.lines() {
                    if l.contains(rest) {
                        println!("{}", l);
                    }
                }
            }
        }
        "ipc" => {
            let n: u64 = rest.parse().unwrap_or(7);
            match crate::ipc::lookup("demo.pong") {
                Some(ep) => {
                    let reply = crate::ipc::call(ep, crate::ipc::Message::new(n)).unwrap_or_else(|_| crate::ipc::Message::default());
                    println!("demo.pong replied with tag {}", reply.tag);
                }
                None => println!("demo.pong endpoint not registered yet"),
            }
        }
        "caps" => {
            let me = task::current_id();
            println!("capabilities for task {}:", me);
            let mut any = false;
            for slot in 0..8 {
                if let Some(cap) = crate::capability::lookup(me, slot) {
                    any = true;
                    println!("  slot {} -> {:?} rights={:#07b}", slot, cap.object, cap.rights.0);
                }
            }
            if !any {
                println!("  (none)");
            }
        }
        "mem" => {
            println!("kernel heap: start={:#x} size={} KiB",
                crate::allocator::HEAP_START,
                crate::allocator::HEAP_SIZE / 1024);
        }
        "irqs" => {
            println!("IRQ line owners and delivered counts:");
            println!("  IRQ  OWNER(task)  DELIVERED");
            for irq in 0..16u8 {
                if let Some((owner, count)) = crate::driver::irq::stats(irq) {
                    if owner != 0 || count != 0 {
                        let label = match irq {
                            0 => " (timer)",
                            1 => " (keyboard)",
                            _ => "",
                        };
                        println!("  {:>3}  {:>11}  {}{}", irq, owner, count, label);
                    }
                }
            }
        }
        "dma" => {
            let (used, total) = crate::driver::dma::stats();
            let pct = if total > 0 { (used * 100) / total } else { 0 };
            println!("DMA pool: {} / {} bytes used ({}%)", used, total, pct);
            println!("  {} MiB pool at phys {:#x}, virt {:#x}",
                total / (1024 * 1024),
                crate::driver::dma::DMA_POOL_PHYS_BASE,
                crate::driver::dma::DMA_POOL_VIRT_BASE);
        }
        "pci" | "lspci" => {
            let devices = crate::driver::pci::list_devices();
            println!("PCI devices ({} found):", devices.len());
            println!("  BUS:DEV.FN  VENDOR:DEVICE  CLASS        DESCRIPTION");
            for dev in devices {
                println!(
                    "  {:02x}:{:02x}.{}     {:04x}:{:04x}     {:02x}:{:02x}:{:02x}   {}",
                    dev.bus,
                    dev.device,
                    dev.function,
                    dev.vendor_id,
                    dev.device_id,
                    dev.class_code,
                    dev.subclass,
                    dev.prog_if,
                    dev.class_name()
                );
            }
        }
        "ahci" | "disks" => {
            let devices = crate::driver::ahci::list_devices();
            if devices.is_empty() {
                println!("No AHCI devices detected (controller not found or no disks attached)");
            } else {
                println!("AHCI devices ({} detected):", devices.len());
                println!("  PORT  TYPE              SIGNATURE");
                for dev in devices {
                    println!(
                        "  {:>4}  {:16}  {:#010x}",
                        dev.index,
                        alloc::format!("{:?}", dev.device_type),
                        dev.signature
                    );
                }
            }
        }
        "identify" => {
            let devices = crate::driver::ahci::list_devices();
            if devices.is_empty() {
                println!("No AHCI devices to query");
            } else {
                for dev in devices {
                    if dev.device_type != crate::driver::ahci::DeviceType::Sata {
                        println!("Port {}: {:?} device (skipping, not SATA)", dev.index, dev.device_type);
                        continue;
                    }
                    println!("Port {}: issuing IDENTIFY DEVICE...", dev.index);
                    match crate::driver::ahci::identify_device(dev.index) {
                        Some(identity) => {
                            let info = crate::driver::ahci::parse_identify(&identity);
                            let size_mb = (info.sectors * info.sector_size as u64) / (1024 * 1024);
                            println!("  Model:       {}", info.model);
                            println!("  Serial:      {}", info.serial);
                            println!("  Sectors:     {} ({} MiB)", info.sectors, size_mb);
                            println!("  Sector size: {} bytes", info.sector_size);
                        }
                        None => {
                            println!("  IDENTIFY DEVICE failed (timeout or error)");
                        }
                    }
                }
            }
        }
        "lsblk" | "blkdev" => {
            let devs = crate::driver::block::list();
            if devs.is_empty() {
                println!("No block devices registered (no SATA disks?)");
            } else {
                println!("Block devices ({}):", devs.len());
                println!("  IDX  NAME    BLKSZ   BLOCKS        CAPACITY");
                for d in devs {
                    let cap_mb = (d.block_size as u64 * d.block_count) / (1024 * 1024);
                    println!(
                        "  {:>3}  {:<6}  {:>5}   {:>11}   {} MiB",
                        d.index, d.name, d.block_size, d.block_count, cap_mb
                    );
                }
            }
        }
        "blkread" => {
            // Usage: blkread <dev> <lba>
            let mut args = rest.split_whitespace();
            match (args.next(), args.next()) {
                (Some(dev_s), Some(lba_s)) => {
                    let idx = resolve_blkdev(dev_s);
                    let lba: u64 = lba_s.parse().unwrap_or(u64::MAX);
                    match (idx, lba) {
                        (Some(idx), lba) if lba != u64::MAX => {
                            match crate::driver::block::read(idx, lba, 1) {
                                Ok(buf) => {
                                    println!("device {} LBA {} (first 256 of {} bytes):",
                                        dev_s, lba, buf.len());
                                    hex_dump(&buf[..core::cmp::min(256, buf.len())]);
                                }
                                Err(e) => println!("read error: {:?}", e),
                            }
                        }
                        (None, _) => println!("unknown device '{}'", dev_s),
                        _ => println!("invalid LBA '{}'", lba_s),
                    }
                }
                _ => println!("usage: blkread <dev> <lba>"),
            }
        }
        "blkwrite" => {
            // Usage: blkwrite <dev> <lba> <string>
            let mut args = rest.splitn(3, char::is_whitespace);
            match (args.next(), args.next(), args.next()) {
                (Some(dev_s), Some(lba_s), Some(text)) => {
                    let idx = resolve_blkdev(dev_s);
                    let lba: u64 = lba_s.parse().unwrap_or(u64::MAX);
                    match (idx, lba) {
                        (Some(idx), lba) if lba != u64::MAX => {
                            // Build a full block: copy text into a zeroed sector.
                            let bs = crate::driver::ahci::SECTOR_SIZE;
                            let mut block = alloc::vec![0u8; bs];
                            let bytes = text.as_bytes();
                            let n = core::cmp::min(bytes.len(), bs);
                            block[..n].copy_from_slice(&bytes[..n]);
                            match crate::driver::block::write(idx, lba, &block) {
                                Ok(()) => println!("wrote {} bytes into {} LBA {} (padded to {} byte block)",
                                    n, dev_s, lba, bs),
                                Err(e) => println!("write error: {:?}", e),
                            }
                        }
                        (None, _) => println!("unknown device '{}'", dev_s),
                        _ => println!("invalid LBA '{}'", lba_s),
                    }
                }
                _ => println!("usage: blkwrite <dev> <lba> <string>"),
            }
        }
        "mkfs.nextfs" => {
            let dev_s = rest.trim();
            if dev_s.is_empty() {
                println!("usage: mkfs.nextfs <dev>");
            } else {
                match resolve_blkdev(dev_s) {
                    Some(idx) => {
                        println!("Formatting {} as NextFS...", dev_s);
                        match crate::fs::NextFS::format(idx) {
                            Ok(()) => println!("Format complete"),
                            Err(e) => println!("Format failed: {}", e),
                        }
                    }
                    None => println!("unknown device '{}'", dev_s),
                }
            }
        }
        "mount" => {
            let dev_s = rest.trim();
            if dev_s.is_empty() {
                println!("usage: mount <dev>");
            } else {
                match resolve_blkdev(dev_s) {
                    Some(idx) => match crate::fs::mount(idx) {
                        Ok(()) => println!("Mounted {} as NextFS", dev_s),
                        Err(e) => println!("Mount failed: {}", e),
                    },
                    None => println!("unknown device '{}'", dev_s),
                }
            }
        }
        "unmount" => {
            crate::fs::unmount();
            println!("Filesystem unmounted");
        }
        "nxls" => {
            let path = if rest.is_empty() { "/" } else { rest };
            match nextfs_list(path) {
                Ok(entries) => {
                    if entries.is_empty() {
                        println!("(empty)");
                    }
                    for e in entries {
                        println!("  {}", e);
                    }
                }
                Err(e) => println!("nxls: {}: {}", path, e),
            }
        }
        "nxcat" => {
            if rest.is_empty() {
                println!("usage: nxcat <path>");
            } else {
                match nextfs_read_file(rest) {
                    Ok(contents) => {
                        let text = alloc::string::String::from_utf8_lossy(&contents);
                        print!("{}", text);
                    }
                    Err(e) => println!("nxcat: {}: {}", rest, e),
                }
            }
        }
        "nxwrite" => {
            let mut parts = rest.splitn(2, char::is_whitespace);
            match (parts.next(), parts.next()) {
                (Some(path), Some(text)) => match nextfs_write_file(path, text.as_bytes()) {
                    Ok(()) => println!("Wrote {} bytes to {}", text.len(), path),
                    Err(e) => println!("nxwrite: {}: {}", path, e),
                },
                _ => println!("usage: nxwrite <path> <text>"),
            }
        }
        "nxmkdir" => {
            if rest.is_empty() {
                println!("usage: nxmkdir <path>");
            } else {
                match nextfs_mkdir(rest) {
                    Ok(()) => println!("Created directory {}", rest),
                    Err(e) => println!("nxmkdir: {}: {}", rest, e),
                }
            }
        }
        "nxrm" => {
            if rest.is_empty() {
                println!("usage: nxrm <path>");
            } else {
                match nextfs_unlink(rest) {
                    Ok(()) => println!("Removed file {}", rest),
                    Err(e) => println!("nxrm: {}: {:?}", rest, e),
                }
            }
        }
        "nxrmdir" => {
            if rest.is_empty() {
                println!("usage: nxrmdir <path>");
            } else {
                match nextfs_rmdir(rest) {
                    Ok(()) => println!("Removed directory {}", rest),
                    Err(e) => println!("nxrmdir: {}: {:?}", rest, e),
                }
            }
        }
        "nxtruncate" => {
            let mut parts = rest.splitn(2, char::is_whitespace);
            match (parts.next(), parts.next()) {
                (Some(path), Some(size_str)) => {
                    match size_str.parse::<u64>() {
                        Ok(size) => match nextfs_truncate(path, size) {
                            Ok(()) => println!("Truncated {} to {} bytes", path, size),
                            Err(e) => println!("nxtruncate: {}: {:?}", path, e),
                        },
                        Err(_) => println!("nxtruncate: invalid size: {}", size_str),
                    }
                }
                _ => println!("usage: nxtruncate <path> <size>"),
            }
        }
        "nxstat" => {
            if rest.is_empty() {
                println!("usage: nxstat <path>");
            } else {
                match nextfs_stat(rest) {
                    Ok(st) => {
                        let modestr = st.mode_string();
                        let kind = if st.is_dir() { "directory" } else { "file" };
                        println!("  path:  {}", rest);
                        println!("  type:  {} (inode {})", kind, st.inode);
                        println!(
                            "  mode:  {} ({:o})",
                            core::str::from_utf8(&modestr).unwrap_or("??????????"),
                            st.mode
                        );
                        println!("  owner: uid={} gid={}", st.uid, st.gid);
                        println!("  size:  {} bytes", st.size);
                        println!("  mtime: {} ticks", st.mtime);
                    }
                    Err(e) => println!("nxstat: {}: {}", rest, e),
                }
            }
        }
        "nxchmod" => {
            let mut parts = rest.splitn(2, char::is_whitespace);
            match (parts.next(), parts.next()) {
                (Some(mode_str), Some(path)) => match u16::from_str_radix(mode_str, 8) {
                    Ok(mode) => match nextfs_chmod(path, mode) {
                        Ok(()) => println!("Mode of {} set to {:o}", path, mode & 0o777),
                        Err(e) => println!("nxchmod: {}: {}", path, e),
                    },
                    Err(_) => println!("nxchmod: invalid octal mode: {}", mode_str),
                },
                _ => println!("usage: nxchmod <octal-mode> <path>"),
            }
        }
        "nxchown" => {
            let mut parts = rest.split_whitespace();
            match (parts.next(), parts.next(), parts.next()) {
                (Some(uid_s), Some(gid_s), Some(path)) => {
                    match (uid_s.parse::<u16>(), gid_s.parse::<u16>()) {
                        (Ok(uid), Ok(gid)) => match nextfs_chown(path, uid, gid) {
                            Ok(()) => println!("Owner of {} set to uid={} gid={}", path, uid, gid),
                            Err(e) => println!("nxchown: {}: {}", path, e),
                        },
                        _ => println!("nxchown: invalid uid/gid"),
                    }
                }
                _ => println!("usage: nxchown <uid> <gid> <path>"),
            }
        }
        "services" => {
            let names = crate::services::registry::list();
            if names.is_empty() {
                println!("registry has no services yet (still booting?)");
            } else {
                println!("registered services:");
                for n in names {
                    println!("  {}", n);
                }
            }
        }
        "ls" => {
            let path = if rest.is_empty() { "/" } else { rest };
            match crate::services::vfs::readdir(path) {
                Ok(entries) => {
                    if entries.is_empty() {
                        println!("(empty)");
                    }
                    for e in entries {
                        // Annotate directories with a trailing slash.
                        let full = if path == "/" {
                            alloc::format!("/{}", e)
                        } else {
                            alloc::format!("{}/{}", path, e)
                        };
                        let is_dir = matches!(
                            crate::services::vfs::stat(&full),
                            Ok(info) if info.is_dir
                        );
                        if is_dir {
                            println!("  {}/", e);
                        } else {
                            println!("  {}", e);
                        }
                    }
                }
                Err(e) => println!("ls: {}: {:?}", path, e),
            }
        }
        "cat" => {
            if rest.is_empty() {
                println!("usage: cat <path>");
            } else {
                match crate::services::vfs::read(rest) {
                    Ok(data) => match core::str::from_utf8(&data) {
                        Ok(s) => print!("{}", s),
                        Err(_) => println!("cat: {}: not valid UTF-8 ({} bytes)", rest, data.len()),
                    },
                    Err(e) => println!("cat: {}: {:?}", rest, e),
                }
            }
        }
        "write" => {
            // write <path> <text...>
            let mut w = rest.splitn(2, char::is_whitespace);
            match (w.next(), w.next()) {
                (Some(path), Some(text)) if !path.is_empty() => {
                    match crate::services::vfs::write(path, text.as_bytes()) {
                        Ok(n) => println!("wrote {} bytes to {}", n, path),
                        Err(e) => println!("write: {}: {:?}", path, e),
                    }
                }
                _ => println!("usage: write <path> <text>"),
            }
        }
        "touch" => {
            if rest.is_empty() {
                println!("usage: touch <path>");
            } else {
                match crate::services::vfs::create(rest) {
                    Ok(()) => println!("created {}", rest),
                    Err(e) => println!("touch: {}: {:?}", rest, e),
                }
            }
        }
        "mkdir" => {
            if rest.is_empty() {
                println!("usage: mkdir <path>");
            } else {
                match crate::services::vfs::mkdir(rest) {
                    Ok(()) => println!("created directory {}", rest),
                    Err(e) => println!("mkdir: {}: {:?}", rest, e),
                }
            }
        }
        "rm" => {
            if rest.is_empty() {
                println!("usage: rm <path>");
            } else {
                match crate::services::vfs::remove(rest) {
                    Ok(()) => println!("removed {}", rest),
                    Err(e) => println!("rm: {}: {:?}", rest, e),
                }
            }
        }
        "stat" => {
            if rest.is_empty() {
                println!("usage: stat <path>");
            } else {
                match crate::services::vfs::stat(rest) {
                    Ok(info) => println!(
                        "  {}: {} size={} bytes",
                        rest,
                        if info.is_dir { "directory" } else { "file" },
                        info.size
                    ),
                    Err(e) => println!("stat: {}: {:?}", rest, e),
                }
            }
        }
        "int3" => {
            println!("triggering breakpoint (int3)...");
            crate::interrupts::trigger_breakpoint();
            println!("...survived the exception, kernel still running.");
        }
        "clear" => {
            if crate::terminal::is_active() {
                // GUI Terminal window: clear this shell's own text grid.
                crate::terminal::clear_for_current();
            } else {
                crate::vga_buffer::WRITER.lock().clear_screen();
            }
        }
        "colors" => {
            const NAMES: [&str; 8] =
                ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];
            println!("ANSI colours (normal 30-37 / bright 90-97):");
            for (i, name) in NAMES.iter().enumerate() {
                // Normal then bright swatch for each colour, reset after each.
                print!("\x1b[{}m  {:<9}\x1b[0m", 30 + i, name);
                print!("\x1b[{}m  {:<9}\x1b[0m", 90 + i, name);
                println!();
            }
            println!(
                "example: \x1b[32mgreen\x1b[0m \x1b[31mred\x1b[0m \x1b[33myellow\x1b[0m \x1b[36mcyan\x1b[0m \x1b[35mmagenta\x1b[0m"
            );
        }
        other => {
            println!("unknown command: '{}' (try 'help')", other);
            serial_println!("[shell] unknown command: {}", other);
        }
    }
}


/// Resolve a block-device argument that is either a registry index (e.g. "0")
/// or a device name (e.g. "sata0") to a registry index.
fn resolve_blkdev(arg: &str) -> Option<usize> {
    if let Ok(idx) = arg.parse::<usize>() {
        if idx < crate::driver::block::device_count() {
            return Some(idx);
        }
    }
    crate::driver::block::find_by_name(arg)
}

/// Print a classic hex + ASCII dump of `data`, 16 bytes per row.
fn hex_dump(data: &[u8]) {
    for (row, chunk) in data.chunks(16).enumerate() {
        // Offset column.
        print!("  {:04x}: ", row * 16);
        // Hex bytes.
        for (i, b) in chunk.iter().enumerate() {
            print!("{:02x} ", b);
            if i == 7 {
                print!(" ");
            }
        }
        // Pad if the final row is short.
        for i in chunk.len()..16 {
            print!("   ");
            if i == 7 {
                print!(" ");
            }
        }
        // ASCII column.
        print!(" |");
        for b in chunk {
            let c = *b;
            if (0x20..0x7f).contains(&c) {
                print!("{}", c as char);
            } else {
                print!(".");
            }
        }
        println!("|");
    }
}


// ---------------------------------------------------------------------------
// NextFS helper functions
// ---------------------------------------------------------------------------

use crate::fs::{FsError, ROOT_INODE};

/// List a NextFS directory. Returns entry names.
fn nextfs_list(path: &str) -> Result<alloc::vec::Vec<alloc::string::String>, FsError> {
    crate::fs::with_fs(|fs| {
        let fs = fs.ok_or(FsError::NotMounted)?;
        let inode = if path == "/" {
            ROOT_INODE
        } else {
            nextfs_resolve_path(fs, path)?
        };
        let entries = fs.dir_list(inode)?;
        Ok(entries.into_iter().map(|(name, _)| name).collect())
    })
}

/// Read a file from NextFS.
fn nextfs_read_file(path: &str) -> Result<alloc::vec::Vec<u8>, FsError> {
    crate::fs::with_fs(|fs| {
        let fs = fs.ok_or(FsError::NotMounted)?;
        let inode = nextfs_resolve_path(fs, path)?;
        fs.read_file(inode)
    })
}

/// Write a file to NextFS (creates if it doesn't exist, overwrites if it does).
fn nextfs_write_file(path: &str, data: &[u8]) -> Result<(), FsError> {
    crate::fs::with_fs_mut(|fs| {
        let fs = fs.ok_or(FsError::NotMounted)?;
        // Split path into parent directory and file name.
        let (parent_path, file_name) = split_path(path);
        let parent_inode = if parent_path == "/" {
            ROOT_INODE
        } else {
            nextfs_resolve_path(fs, parent_path)?
        };

        // Try to find existing file.
        match fs.dir_lookup(parent_inode, file_name) {
            Ok(inode) => {
                // File exists; overwrite it.
                fs.write_file(inode, data)?;
            }
            Err(FsError::NotFound) => {
                // File doesn't exist; create it.
                let new_inode = fs.create_file(parent_inode, file_name)?;
                fs.write_file(new_inode, data)?;
            }
            Err(e) => return Err(e),
        }
        fs.sync()
    })
}

/// Create a directory in NextFS.
fn nextfs_mkdir(path: &str) -> Result<(), FsError> {
    crate::fs::with_fs_mut(|fs| {
        let fs = fs.ok_or(FsError::NotMounted)?;
        let (parent_path, dir_name) = split_path(path);
        let parent_inode = if parent_path == "/" {
            ROOT_INODE
        } else {
            nextfs_resolve_path(fs, parent_path)?
        };
        fs.create_dir(parent_inode, dir_name)?;
        fs.sync()
    })
}

/// Unlink (delete) a file from NextFS.
fn nextfs_unlink(path: &str) -> Result<(), FsError> {
    crate::fs::with_fs_mut(|fs| {
        let fs = fs.ok_or(FsError::NotMounted)?;
        let (parent_path, file_name) = split_path(path);
        let parent_inode = if parent_path == "/" {
            ROOT_INODE
        } else {
            nextfs_resolve_path(fs, parent_path)?
        };
        fs.unlink(parent_inode, file_name)?;
        fs.sync()
    })
}

/// Remove a directory from NextFS.
fn nextfs_rmdir(path: &str) -> Result<(), FsError> {
    crate::fs::with_fs_mut(|fs| {
        let fs = fs.ok_or(FsError::NotMounted)?;
        let (parent_path, dir_name) = split_path(path);
        let parent_inode = if parent_path == "/" {
            ROOT_INODE
        } else {
            nextfs_resolve_path(fs, parent_path)?
        };
        fs.rmdir(parent_inode, dir_name)?;
        fs.sync()
    })
}

/// Truncate a file in NextFS.
fn nextfs_truncate(path: &str, size: u64) -> Result<(), FsError> {
    crate::fs::with_fs_mut(|fs| {
        let fs = fs.ok_or(FsError::NotMounted)?;
        let inode = nextfs_resolve_path(fs, path)?;
        fs.truncate(inode, size)?;
        fs.sync()
    })
}

/// Stat a path in NextFS.
fn nextfs_stat(path: &str) -> Result<crate::fs::FileStat, FsError> {
    crate::fs::with_fs(|fs| {
        let fs = fs.ok_or(FsError::NotMounted)?;
        let inode = nextfs_resolve_path(fs, path)?;
        fs.stat(inode)
    })
}

/// Change permission bits of a path in NextFS.
fn nextfs_chmod(path: &str, mode: u16) -> Result<(), FsError> {
    crate::fs::with_fs_mut(|fs| {
        let fs = fs.ok_or(FsError::NotMounted)?;
        let inode = nextfs_resolve_path(fs, path)?;
        fs.chmod(inode, mode)?;
        fs.sync()
    })
}

/// Change owner/group of a path in NextFS.
fn nextfs_chown(path: &str, uid: u16, gid: u16) -> Result<(), FsError> {
    crate::fs::with_fs_mut(|fs| {
        let fs = fs.ok_or(FsError::NotMounted)?;
        let inode = nextfs_resolve_path(fs, path)?;
        fs.chown(inode, uid, gid)?;
        fs.sync()
    })
}

/// Resolve an absolute path to an inode number.
fn nextfs_resolve_path(fs: &crate::fs::NextFS, path: &str) -> Result<u32, FsError> {
    if path == "/" {
        return Ok(ROOT_INODE);
    }
    let mut current = ROOT_INODE;
    let parts: alloc::vec::Vec<&str> = path.trim_start_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    for part in parts {
        current = fs.dir_lookup(current, part)?;
    }
    Ok(current)
}

/// Boot-time self-test for the shell pipe (`|`) and redirect (`>`) operators.
///
/// The interactive shell only renders to the VGA text buffer, which is invisible
/// in a headless serial boot. To get automated, serial-visible proof that the
/// pipe and redirect machinery works, this routine drives `run_command_line`
/// directly and reads the results back from the filesystem, reporting via
/// `serial_println!` with the established `[selftest]` prefix.
pub fn pipe_redirect_selftest() {
    serial_println!("[selftest] shell pipe/redirect: begin");

    // 1) Plain redirect: `echo` output captured to a file.
    run_command_line("echo pipe_hello > /selftest_echo.txt");
    match nextfs_read_file("/selftest_echo.txt") {
        Ok(bytes) => {
            let text = core::str::from_utf8(&bytes).unwrap_or("<non-utf8>");
            let trimmed = text.trim_end();
            if trimmed == "pipe_hello" {
                serial_println!("[selftest] redirect OK: '/selftest_echo.txt' = {:?}", trimmed);
            } else {
                serial_println!("[selftest] redirect FAIL: got {:?}", trimmed);
            }
        }
        Err(e) => serial_println!("[selftest] redirect FAIL: read error {}", e),
    }

    // 2) Pipe data-flow: feed `ps` output through the `wc` filter, exactly as the
    //    `|` operator does in `run_command_line` (capture stage N's stdout, hand it
    //    to stage N+1 as input). This is verified purely in memory so it does not
    //    depend on the flaky AHCI/NextFS DMA path during early boot.
    crate::vga_buffer::capture_begin();
    execute("ps", None);
    let ps_out = crate::vga_buffer::capture_end().unwrap_or_default();

    crate::vga_buffer::capture_begin();
    execute("wc", Some(&ps_out));
    let wc_out = crate::vga_buffer::capture_end().unwrap_or_default();

    let trimmed = wc_out.trim();
    // `wc` emits "<lines> <words> <bytes>"; a non-empty 3-field line proves the
    // `ps` output actually flowed into `wc` through the pipe capture/feed path.
    let fields = trimmed.split_whitespace().count();
    if fields == 3 && !ps_out.is_empty() {
        serial_println!("[selftest] pipe OK: 'ps | wc' -> {:?} (ps produced {} bytes)", trimmed, ps_out.len());
    } else {
        serial_println!("[selftest] pipe FAIL: ps={} bytes, wc={:?}", ps_out.len(), trimmed);
    }

    serial_println!("[selftest] shell pipe/redirect: done");
}

/// Split a path into (parent_path, file_name).
fn split_path(path: &str) -> (&str, &str) {
    let path = path.trim_end_matches('/');
    if let Some(pos) = path.rfind('/') {
        let parent = if pos == 0 { "/" } else { &path[..pos] };
        let name = &path[pos + 1..];
        (parent, name)
    } else {
        ("/", path)
    }
}
