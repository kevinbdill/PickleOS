#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;
use libpickleos::{syscall, gui::{Window, Event}, widgets::Color, font, println};

// The file the editor loads/saves. Kept simple: a single fixed scratch file.
const EDIT_PATH: &str = "/tmp/scratch.txt";

// The compositor caps window size at 320x240; stay within that.
const WIN_W: u32 = 320;
const WIN_H: u32 = 240;
const MARGIN: i32 = 8;
const LINE_H: i32 = 10;
const TOP_BAR: i32 = 24;
const BOTTOM_BAR: i32 = 22;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    if let Err(e) = libpickleos::init_heap() {
        syscall::sys_print("texteditor: heap init failed: ");
        syscall::sys_print(e);
        syscall::sys_print("\n");
        syscall::sys_exit(1);
    }

    println!("[texteditor] Starting Text Editor");

    match run() {
        Ok(_) => syscall::sys_exit(0),
        Err(e) => {
            println!("[texteditor] Error: {}", e);
            syscall::sys_exit(1)
        }
    }
}

struct Editor {
    window: Window,
    // Document stored as lines of text.
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
    scroll_row: usize,
    status: String,
    dirty: bool,
}

impl Editor {
    fn new() -> Result<Self, &'static str> {
        let window = Window::create(WIN_W, WIN_H, "Text Editor").ok_or("window create failed")?;
        let mut ed = Editor {
            window,
            lines: Vec::new(),
            cursor_row: 0,
            cursor_col: 0,
            scroll_row: 0,
            status: String::from("Ctrl-S=Save  ESC=Quit"),
            dirty: false,
        };
        ed.load();
        if ed.lines.is_empty() {
            ed.lines.push(String::new());
        }
        Ok(ed)
    }

    fn load(&mut self) {
        let fd = syscall::sys_open(EDIT_PATH, syscall::O_RDONLY);
        if fd == u64::MAX {
            self.status = format!("New file: {}", EDIT_PATH);
            return;
        }
        let fd = fd as u32;
        let mut data: Vec<u8> = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            let n = syscall::sys_read(fd, &mut buf);
            if n == u64::MAX || n == 0 {
                break;
            }
            data.extend_from_slice(&buf[..n as usize]);
        }
        syscall::sys_close(fd);

        let text = String::from_utf8_lossy(&data);
        self.lines = text.split('\n').map(String::from).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.status = format!("Loaded {} bytes", data.len());
    }

    fn save(&mut self) {
        let flags = syscall::O_WRONLY | syscall::O_CREAT | syscall::O_TRUNC;
        let fd = syscall::sys_open(EDIT_PATH, flags);
        if fd == u64::MAX {
            self.status = String::from("Save failed: open error");
            return;
        }
        let fd = fd as u32;
        let mut content = self.lines.join("\n");
        content.push('\n');
        let bytes = content.as_bytes();
        let mut written = 0usize;
        while written < bytes.len() {
            let n = syscall::sys_write(fd, &bytes[written..]);
            if n == u64::MAX || n == 0 {
                break;
            }
            written += n as usize;
        }
        syscall::sys_close(fd);
        self.dirty = false;
        self.status = format!("Saved {} bytes to {}", written, EDIT_PATH);
    }

    fn visible_rows(&self) -> usize {
        ((WIN_H as i32 - TOP_BAR - BOTTOM_BAR) / LINE_H) as usize
    }

    fn ensure_cursor_visible(&mut self) {
        let vis = self.visible_rows();
        if self.cursor_row < self.scroll_row {
            self.scroll_row = self.cursor_row;
        } else if self.cursor_row >= self.scroll_row + vis {
            self.scroll_row = self.cursor_row - vis + 1;
        }
    }

    fn draw(&mut self) {
        let bg = Color::rgb(30, 30, 40).to_u32();
        let fg = Color::WHITE.to_u32();
        self.window.clear(bg);

        // Top bar
        self.window.fill_rect(0, 0, WIN_W as i32, TOP_BAR, Color::BLUE.to_u32());
        let title = if self.dirty { "Text Editor *" } else { "Text Editor" };
        font::draw_string(&mut self.window, title, MARGIN, 8, Color::WHITE.to_u32());

        // Text region
        let vis = self.visible_rows();
        let start = self.scroll_row;
        let end = (start + vis).min(self.lines.len());
        for (i, row) in (start..end).enumerate() {
            let y = TOP_BAR + 4 + (i as i32 * LINE_H);
            let line = self.lines[row].clone();
            font::draw_string(&mut self.window, &line, MARGIN, y, fg);

            // Draw cursor on the active line
            if row == self.cursor_row {
                let cx = MARGIN + (self.cursor_col as i32 * font::GLYPH_W);
                self.window.fill_rect(cx, y, 1, font::GLYPH_H, Color::YELLOW.to_u32());
            }
        }

        // Bottom status bar
        let by = WIN_H as i32 - BOTTOM_BAR;
        self.window.fill_rect(0, by, WIN_W as i32, BOTTOM_BAR, Color::DARK_GRAY.to_u32());
        let status = format!(
            "L{}:C{}  {}",
            self.cursor_row + 1,
            self.cursor_col + 1,
            self.status
        );
        font::draw_string(&mut self.window, &status, MARGIN, by + 6, Color::WHITE.to_u32());

        self.window.commit();
    }

    fn insert_char(&mut self, ch: char) {
        let line = &mut self.lines[self.cursor_row];
        let byte_idx = char_to_byte(line, self.cursor_col);
        line.insert(byte_idx, ch);
        self.cursor_col += 1;
        self.dirty = true;
    }

    fn newline(&mut self) {
        let line = self.lines[self.cursor_row].clone();
        let byte_idx = char_to_byte(&line, self.cursor_col);
        let (left, right) = line.split_at(byte_idx);
        self.lines[self.cursor_row] = String::from(left);
        self.lines.insert(self.cursor_row + 1, String::from(right));
        self.cursor_row += 1;
        self.cursor_col = 0;
        self.dirty = true;
    }

    fn backspace(&mut self) {
        if self.cursor_col > 0 {
            let line = &mut self.lines[self.cursor_row];
            let byte_idx = char_to_byte(line, self.cursor_col - 1);
            line.remove(byte_idx);
            self.cursor_col -= 1;
            self.dirty = true;
        } else if self.cursor_row > 0 {
            // Merge with previous line
            let cur = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].chars().count();
            self.lines[self.cursor_row].push_str(&cur);
            self.dirty = true;
        }
    }

    fn line_len(&self, row: usize) -> usize {
        self.lines[row].chars().count()
    }

    fn handle_key(&mut self, key: u8) -> bool {
        match key {
            0x1B => return true, // ESC = quit
            0x13 => self.save(),  // Ctrl-S (DC3)
            b'\n' | b'\r' => self.newline(),
            0x08 | 0x7F => self.backspace(),
            // Arrow keys arrive as raw bytes; emulate with vim-ish fallback keys
            // if the terminal sends them. We support a few control bytes.
            ch if ch >= 0x20 && ch < 0x7F => self.insert_char(ch as char),
            _ => {}
        }
        self.ensure_cursor_visible();
        false
    }

    fn run(&mut self) -> Result<(), &'static str> {
        loop {
            self.draw();
            while let Some(ev) = self.window.poll_event() {
                match ev {
                    Event::Key(k) => {
                        if self.handle_key(k) {
                            return Ok(());
                        }
                    }
                    Event::Close => return Ok(()),
                    Event::MouseDown { x, y } => {
                        // Move cursor to click position
                        if y >= TOP_BAR && y < WIN_H as i32 - BOTTOM_BAR {
                            let row = self.scroll_row
                                + ((y - TOP_BAR - 4) / LINE_H).max(0) as usize;
                            if row < self.lines.len() {
                                self.cursor_row = row;
                                let col = ((x - MARGIN) / font::GLYPH_W).max(0) as usize;
                                self.cursor_col = col.min(self.line_len(row));
                            }
                        }
                    }
                    _ => {}
                }
            }
            syscall::sys_yield();
        }
    }
}

/// Convert a character index into a byte index within `s` (UTF-8 safe).
fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

fn run() -> Result<(), &'static str> {
    let mut ed = Editor::new()?;
    ed.run()
}
