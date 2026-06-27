#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::format;
use libpickleos::{syscall, gui::{Window, Event}, widgets::Color, font, process, println};

// The compositor caps window size at 320x240; stay within that.
const WIN_W: u32 = 320;
const WIN_H: u32 = 240;

// Icon grid layout.
const COLS: i32 = 2;
const ICON_W: i32 = 130;
const ICON_H: i32 = 70;
const PAD_X: i32 = 20;
const PAD_Y: i32 = 20;
const TOP_BAR: i32 = 28;
const STATUS_H: i32 = 22;

/// An entry in the launcher: a display label and the program path to exec.
struct AppEntry {
    label: &'static str,
    path: &'static str,
}

const APPS: &[AppEntry] = &[
    AppEntry { label: "Files", path: "/bin/filemanager" },
    AppEntry { label: "Editor", path: "/bin/texteditor" },
    AppEntry { label: "Calculator", path: "/bin/calculator" },
    AppEntry { label: "Taskbar", path: "/bin/taskbar" },
];

#[no_mangle]
pub extern "C" fn _start() -> ! {
    if let Err(e) = libpickleos::init_heap() {
        syscall::sys_print("launcher: heap init failed: ");
        syscall::sys_print(e);
        syscall::sys_print("\n");
        syscall::sys_exit(1);
    }

    println!("[launcher] Starting Application Launcher");

    match run() {
        Ok(_) => syscall::sys_exit(0),
        Err(e) => {
            println!("[launcher] Error: {}", e);
            syscall::sys_exit(1)
        }
    }
}

struct Launcher {
    window: Window,
    hovered: Option<usize>,
    status: String,
}

impl Launcher {
    fn new() -> Result<Self, &'static str> {
        let window = Window::create(WIN_W, WIN_H, "Launcher").ok_or("window create failed")?;
        Ok(Launcher {
            window,
            hovered: None,
            status: String::from("Click an app to launch"),
        })
    }

    /// Bounding box of icon `i`: (x, y, w, h).
    fn icon_rect(i: usize) -> (i32, i32, i32, i32) {
        let col = (i as i32) % COLS;
        let row = (i as i32) / COLS;
        let x = PAD_X + col * (ICON_W + PAD_X);
        let y = TOP_BAR + PAD_Y + row * (ICON_H + PAD_Y);
        (x, y, ICON_W, ICON_H)
    }

    fn hit_test(&self, mx: i32, my: i32) -> Option<usize> {
        for i in 0..APPS.len() {
            let (x, y, w, h) = Self::icon_rect(i);
            if mx >= x && mx < x + w && my >= y && my < y + h {
                return Some(i);
            }
        }
        None
    }

    fn draw(&mut self) {
        self.window.clear(Color::rgb(24, 28, 40).to_u32());

        // Title bar
        self.window.fill_rect(0, 0, WIN_W as i32, TOP_BAR, Color::BLUE.to_u32());
        font::draw_string(&mut self.window, "PickleOS Launcher", 10, 10, Color::WHITE.to_u32());

        // Icons
        for (i, app) in APPS.iter().enumerate() {
            let (x, y, w, h) = Self::icon_rect(i);
            let bg = if Some(i) == self.hovered {
                Color::rgb(80, 110, 200)
            } else {
                Color::rgb(50, 60, 90)
            };
            self.window.fill_rect(x, y, w, h, bg.to_u32());

            // Border
            self.draw_border(x, y, w, h, Color::LIGHT_GRAY.to_u32());

            // Icon glyph (a simple square) centered near top
            let icon_size = 24;
            let ix = x + (w - icon_size) / 2;
            let iy = y + 8;
            self.window.fill_rect(ix, iy, icon_size, icon_size, Color::YELLOW.to_u32());

            // Label centered below the icon
            let label_w = app.label.len() as i32 * font::GLYPH_W;
            let lx = x + (w - label_w) / 2;
            let ly = y + h - 18;
            font::draw_string(&mut self.window, app.label, lx, ly, Color::WHITE.to_u32());
        }

        // Status bar
        let by = WIN_H as i32 - STATUS_H;
        self.window.fill_rect(0, by, WIN_W as i32, STATUS_H, Color::DARK_GRAY.to_u32());
        let status = self.status.clone();
        font::draw_string(&mut self.window, &status, 8, by + 6, Color::WHITE.to_u32());

        self.window.commit();
    }

    fn draw_border(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        self.window.fill_rect(x, y, w, 1, color);
        self.window.fill_rect(x, y + h - 1, w, 1, color);
        self.window.fill_rect(x, y, 1, h, color);
        self.window.fill_rect(x + w - 1, y, 1, h, color);
    }

    fn launch(&mut self, idx: usize) {
        let app = &APPS[idx];
        match process::spawn_path(app.path) {
            Some(pid) => {
                self.status = format!("Launched {} (pid {})", app.label, pid);
            }
            None => {
                self.status = format!("Failed to launch {}", app.label);
            }
        }
    }

    fn run(&mut self) -> Result<(), &'static str> {
        loop {
            self.draw();
            while let Some(ev) = self.window.poll_event() {
                match ev {
                    Event::MouseMove { x, y } => {
                        self.hovered = self.hit_test(x, y);
                    }
                    Event::MouseDown { x, y } => {
                        if let Some(idx) = self.hit_test(x, y) {
                            self.launch(idx);
                        }
                    }
                    Event::Key(k) => {
                        if k == 0x1B {
                            return Ok(()); // ESC quits
                        }
                    }
                    Event::Close => return Ok(()),
                    _ => {}
                }
            }
            // Reap any finished children so they don't linger as zombies.
            while process::wait().is_some() {}
            syscall::sys_yield();
        }
    }
}

fn run() -> Result<(), &'static str> {
    let mut launcher = Launcher::new()?;
    launcher.run()
}
