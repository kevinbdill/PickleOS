#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::format;
use libpickleos::{syscall, gui::{Window, Event}, widgets::Color, font, process, println};

// A horizontal taskbar pinned across the top of the desktop.
// The compositor caps window size at 320x240, so keep within that.
const WIN_W: u32 = 320;
const WIN_H: u32 = 32;

// Legacy PIT runs at ~18.2 Hz on this platform.
const TICKS_PER_SEC: u64 = 18;

// Button geometry.
const BTN_H: i32 = 24;
const BTN_Y: i32 = 4;
const BTN_PAD: i32 = 6;

/// A clickable item in the taskbar: a label and the program it launches.
struct TaskButton {
    label: &'static str,
    path: &'static str,
    width: i32,
}

const BUTTONS: &[TaskButton] = &[
    TaskButton { label: "Menu",  path: "/bin/launcher",    width: 56 },
    TaskButton { label: "Files", path: "/bin/filemanager", width: 56 },
    TaskButton { label: "Edit",  path: "/bin/texteditor",  width: 52 },
    TaskButton { label: "Calc",  path: "/bin/calculator",  width: 52 },
];

#[no_mangle]
pub extern "C" fn _start() -> ! {
    if let Err(e) = libpickleos::init_heap() {
        syscall::sys_print("taskbar: heap init failed: ");
        syscall::sys_print(e);
        syscall::sys_print("\n");
        syscall::sys_exit(1);
    }

    println!("[taskbar] Starting Taskbar");

    match run() {
        Ok(_) => syscall::sys_exit(0),
        Err(e) => {
            println!("[taskbar] Error: {}", e);
            syscall::sys_exit(1)
        }
    }
}

struct Taskbar {
    window: Window,
    pid: u64,
    hovered: Option<usize>,
    last_launch: String,
    launches: u32,
}

impl Taskbar {
    fn new() -> Result<Self, &'static str> {
        let window = Window::create(WIN_W, WIN_H, "Taskbar").ok_or("window create failed")?;
        Ok(Taskbar {
            window,
            pid: process::getpid(),
            hovered: None,
            last_launch: String::from("ready"),
            launches: 0,
        })
    }

    /// X position where the button at `i` begins.
    fn button_x(i: usize) -> i32 {
        let mut x = BTN_PAD;
        for b in BUTTONS.iter().take(i) {
            x += b.width + BTN_PAD;
        }
        x
    }

    fn hit_test(&self, mx: i32, my: i32) -> Option<usize> {
        if my < BTN_Y || my >= BTN_Y + BTN_H {
            return None;
        }
        for i in 0..BUTTONS.len() {
            let x = Self::button_x(i);
            let w = BUTTONS[i].width;
            if mx >= x && mx < x + w {
                return Some(i);
            }
        }
        None
    }

    fn draw_border(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        self.window.fill_rect(x, y, w, 1, color);
        self.window.fill_rect(x, y + h - 1, w, 1, color);
        self.window.fill_rect(x, y, 1, h, color);
        self.window.fill_rect(x + w - 1, y, 1, h, color);
    }

    fn draw(&mut self) {
        // Bar background.
        self.window.clear(Color::rgb(30, 34, 48).to_u32());
        self.window.fill_rect(0, 0, WIN_W as i32, 1, Color::rgb(70, 80, 110).to_u32());

        // Launch buttons.
        for (i, b) in BUTTONS.iter().enumerate() {
            let x = Self::button_x(i);
            let bg = if Some(i) == self.hovered {
                Color::rgb(80, 110, 200)
            } else {
                Color::rgb(54, 64, 92)
            };
            self.window.fill_rect(x, BTN_Y, b.width, BTN_H, bg.to_u32());
            self.draw_border(x, BTN_Y, b.width, BTN_H, Color::rgb(100, 115, 150).to_u32());

            let label_w = b.label.len() as i32 * font::GLYPH_W;
            let lx = x + (b.width - label_w) / 2;
            let ly = BTN_Y + (BTN_H - font::GLYPH_H) / 2;
            font::draw_string(&mut self.window, b.label, lx, ly, Color::WHITE.to_u32());
        }

        // Right-hand status: compact uptime clock (full width is only 320px).
        let ticks = syscall::sys_ticks();
        let secs = ticks / TICKS_PER_SEC;
        let mins = secs / 60;
        let _ = (self.launches, self.last_launch.as_str());
        let info = format!("{:02}:{:02}", mins, secs % 60);
        let info_w = info.len() as i32 * font::GLYPH_W;
        let ix = WIN_W as i32 - info_w - 6;
        let iy = BTN_Y + (BTN_H - font::GLYPH_H) / 2;
        font::draw_string(&mut self.window, &info, ix, iy, Color::rgb(180, 200, 230).to_u32());

        self.window.commit();
    }

    fn launch(&mut self, idx: usize) {
        let b = &BUTTONS[idx];
        match process::spawn_path(b.path) {
            Some(pid) => {
                self.launches += 1;
                self.last_launch = format!("{} #{}", b.label, pid);
                println!("[taskbar] launched {} (pid {})", b.label, pid);
            }
            None => {
                self.last_launch = format!("fail {}", b.label);
                println!("[taskbar] failed to launch {}", b.label);
            }
        }
    }

    fn run(&mut self) -> Result<(), &'static str> {
        let mut frame: u64 = 0;
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

            // Reap finished children so they don't linger as zombies.
            while process::wait().is_some() {}

            // Refresh the clock roughly twice a second without busy-spinning.
            frame += 1;
            syscall::sys_sleep(9);
            let _ = frame;
        }
    }
}

fn run() -> Result<(), &'static str> {
    let mut taskbar = Taskbar::new()?;
    taskbar.run()
}
