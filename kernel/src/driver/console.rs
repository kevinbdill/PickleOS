//! Console input line discipline — the single keyboard decode point.
//!
//! Historically the shell owned both the raw scancode queue *and* the
//! `pc_keyboard` decoder. That made it impossible for anything other than the
//! shell (e.g. a user program reading `stdin`) to receive keyboard input.
//!
//! This module centralises decoding: the keyboard driver task feeds raw
//! scancodes in via [`feed_scancode`], we decode them into Unicode characters
//! using a single shared `pc_keyboard` state machine, and the decoded
//! characters land in a small ring buffer. Both the in-kernel shell and the
//! VFS `stdin` path drain that buffer through [`read_char`].
//!
//! Reads are non-blocking: [`read_char`] returns `None` when no input is
//! buffered. The VFS `read(0, …)` path therefore implements POSIX-style
//! "return what's available" semantics (returning 0 only at a true would-block
//! point), which keeps keyboard handling out of the syscall fast path.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use pc_keyboard::{layouts, DecodedKey, HandleControl, KeyCode, Keyboard, ScancodeSet1};
use spin::Mutex;

/// Sentinel characters used to deliver non-Unicode editing keys (arrows, Home,
/// End, Delete) through the same `char` stream as ordinary text. They occupy
/// otherwise-unused ASCII control slots: with `HandleControl::Ignore` the
/// decoder never emits these for normal typing, so they are unambiguous. The
/// shell's line editor interprets them; programs that don't care can ignore
/// them exactly as they would any control byte.
pub mod keys {
    pub const ARROW_UP: char = '\u{11}'; // DC1
    pub const ARROW_DOWN: char = '\u{12}'; // DC2
    pub const ARROW_LEFT: char = '\u{13}'; // DC3
    pub const ARROW_RIGHT: char = '\u{14}'; // DC4
    pub const HOME: char = '\u{15}'; // NAK
    pub const END: char = '\u{16}'; // SYN
    pub const PAGE_UP: char = '\u{17}'; // ETB (scroll terminal back)
    pub const PAGE_DOWN: char = '\u{18}'; // CAN (scroll terminal forward)
    pub const DELETE: char = '\u{7f}'; // DEL (forward delete)
}

/// Capacity of the decoded-character ring buffer.
const CAP: usize = 256;

/// Decoded-character ring buffer plus the shared keyboard decoder state.
struct ConsoleInput {
    decoder: Keyboard<layouts::Us104Key, ScancodeSet1>,
    buf: [char; CAP],
    head: usize,
    tail: usize,
    /// Tasks blocked waiting for stdin input.
    waiters: Vec<u32>,
}

static INPUT: Mutex<ConsoleInput> = Mutex::new(ConsoleInput {
    decoder: Keyboard::new(ScancodeSet1::new(), layouts::Us104Key, HandleControl::Ignore),
    buf: ['\0'; CAP],
    head: 0,
    tail: 0,
    waiters: Vec::new(),
});

/// Number of characters dropped because the buffer was full (diagnostic).
static DROPPED: AtomicUsize = AtomicUsize::new(0);

/// Feed one raw PS/2 scancode. Decodes it and, if a printable/Unicode key
/// resulted, enqueues the character. Called from the keyboard driver task.
pub fn feed_scancode(code: u8) {
    let mut input = INPUT.lock();
    if let Ok(Some(event)) = input.decoder.add_byte(code) {
        if let Some(key) = input.decoder.process_keyevent(event) {
            match key {
                DecodedKey::Unicode(c) => {
                    push_char(&mut input, c);
                    wake_waiters(&mut input);
                }
                // Map the editing keys we care about to sentinel control chars
                // so the shell's line editor can act on them. Other raw keys
                // (F-keys, modifiers) are still ignored.
                DecodedKey::RawKey(code) => {
                    if let Some(c) = raw_key_to_sentinel(code) {
                        push_char(&mut input, c);
                        wake_waiters(&mut input);
                    }
                }
            }
        }
    }
}

/// Translate the navigation/editing raw key codes into the sentinel characters
/// defined in [`keys`]. Returns `None` for keys we don't forward.
fn raw_key_to_sentinel(code: KeyCode) -> Option<char> {
    Some(match code {
        KeyCode::ArrowUp => keys::ARROW_UP,
        KeyCode::ArrowDown => keys::ARROW_DOWN,
        KeyCode::ArrowLeft => keys::ARROW_LEFT,
        KeyCode::ArrowRight => keys::ARROW_RIGHT,
        KeyCode::Home => keys::HOME,
        KeyCode::End => keys::END,
        KeyCode::PageUp => keys::PAGE_UP,
        KeyCode::PageDown => keys::PAGE_DOWN,
        KeyCode::Delete => keys::DELETE,
        _ => return None,
    })
}

/// Internal: push a decoded character into the ring buffer.
fn push_char(input: &mut ConsoleInput, c: char) {
    let next = (input.head + 1) % CAP;
    if next == input.tail {
        DROPPED.fetch_add(1, Ordering::Relaxed);
        return; // full — drop
    }
    let h = input.head;
    input.buf[h] = c;
    input.head = next;
}

/// Pop the next decoded character, or `None` if the buffer is empty.
pub fn read_char() -> Option<char> {
    let mut input = INPUT.lock();
    if input.head == input.tail {
        return None;
    }
    let t = input.tail;
    let c = input.buf[t];
    input.tail = (t + 1) % CAP;
    Some(c)
}

/// True if at least one decoded character is buffered.
pub fn has_input() -> bool {
    let input = INPUT.lock();
    input.head != input.tail
}

/// Discard any buffered input. The compositor calls this when keyboard focus
/// moves between terminals so a freshly-focused shell does not suddenly receive
/// keystrokes the user typed while a different window was focused.
pub fn flush() {
    let mut input = INPUT.lock();
    input.tail = input.head;
}

/// Injects a character directly into the input buffer. Used by boot-time
/// self-tests to exercise the `stdin` read path deterministically without a
/// physical keyboard.
pub fn inject_char(c: char) {
    let mut input = INPUT.lock();
    push_char(&mut input, c);
    wake_waiters(&mut input);
}

/// Internal: wake all tasks waiting for stdin input.
fn wake_waiters(input: &mut ConsoleInput) {
    for &task_id in &input.waiters {
        crate::task::unblock(task_id as u64);
    }
    input.waiters.clear();
}

/// Block the current task until at least one character is available, then
/// return it. Used by VFS stdin reads to implement proper blocking semantics.
pub fn read_char_blocking() -> char {
    loop {
        let mut input = INPUT.lock();
        if input.head != input.tail {
            // Data available - read and return it.
            let t = input.tail;
            let c = input.buf[t];
            input.tail = (t + 1) % CAP;
            return c;
        }
        // Buffer empty - register as waiter and block.
        let task_id = crate::task::current_id() as u32;
        if !input.waiters.contains(&task_id) {
            input.waiters.push(task_id);
        }
        drop(input); // Release lock before blocking.
        crate::task::block_current_and_schedule();
    }
}
