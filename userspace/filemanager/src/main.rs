#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;
use libpickleos::{syscall, gui::{Window, Event}, widgets::*, println};

// Entry point
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Initialize heap allocator
    if let Err(e) = libpickleos::init_heap() {
        syscall::sys_print("Failed to init heap: ");
        syscall::sys_print(e);
        syscall::sys_print("\n");
        syscall::sys_exit(1);
    }
    
    println!("[filemanager] Starting File Manager");
    
    match run_file_manager() {
        Ok(_) => syscall::sys_exit(0),
        Err(e) => {
            println!("[filemanager] Error: {}", e);
            syscall::sys_exit(1)
        }
    }
}

#[derive(Clone)]
struct FileEntry {
    name: String,
    is_dir: bool,
    size: u64,
}

struct FileManager {
    window: Window,
    current_path: String,
    entries: Vec<FileEntry>,
    selected_index: Option<usize>,
    scroll_offset: usize,
    status_msg: String,
}

impl FileManager {
    fn new() -> Result<Self, &'static str> {
        // The compositor caps window size at 320x240; stay within that.
        let window = Window::create(320, 240, "File Manager").ok_or("Failed to create window")?;
        
        let mut fm = FileManager {
            window,
            current_path: String::from("/"),
            entries: Vec::new(),
            selected_index: None,
            scroll_offset: 0,
            status_msg: String::from("Ready"),
        };
        
        fm.load_directory()?;
        Ok(fm)
    }
    
    fn load_directory(&mut self) -> Result<(), &'static str> {
        self.entries.clear();
        self.selected_index = None;
        self.scroll_offset = 0;
        
        // Add parent directory entry if not at root
        if self.current_path != "/" {
            self.entries.push(FileEntry {
                name: String::from(".."),
                is_dir: true,
                size: 0,
            });
        }
        
        // Read directory contents by trying to open files
        // Since we don't have a readdir syscall exposed yet, we'll use stat
        // to check common paths
        let test_names = ["bin", "etc", "tmp", "root", "home", "dev", "proc", "sys"];
        
        for name in &test_names {
            let mut path = self.current_path.clone();
            if !path.ends_with('/') {
                path.push('/');
            }
            path.push_str(name);
            
            let mut statbuf = [0u8; 24];
            if syscall::sys_stat(&path, &mut statbuf) != u64::MAX {
                let size = u64::from_le_bytes([
                    statbuf[0], statbuf[1], statbuf[2], statbuf[3],
                    statbuf[4], statbuf[5], statbuf[6], statbuf[7],
                ]);
                let is_dir = statbuf[8] != 0;
                
                self.entries.push(FileEntry {
                    name: String::from(*name),
                    is_dir,
                    size,
                });
            }
        }
        
        self.status_msg = format!("Loaded {} items", self.entries.len());
        Ok(())
    }
    
    fn draw(&mut self) {
        let (w, h) = self.window.size();
        
        // Clear background
        self.window.clear(Color::LIGHT_GRAY.to_u32());
        
        // Draw title bar
        self.window.fill_rect(0, 0, w as i32, 30, Color::BLUE.to_u32());
        self.draw_text(&format!("Path: {}", self.current_path), 5, 8, Color::WHITE);
        
        // Draw file list area
        self.window.fill_rect(0, 30, w as i32, (h as i32) - 60, Color::WHITE.to_u32());
        
        // Draw entries
        let visible_items = ((h - 60) / 20).min(20);
        
        // Collect display data without holding borrow
        let entries_to_draw: Vec<(usize, FileEntry)> = self.entries.iter()
            .skip(self.scroll_offset)
            .take(visible_items as usize)
            .enumerate()
            .map(|(i, e)| (i, e.clone()))
            .collect();
        
        for (i, entry) in entries_to_draw {
            let y = 30 + (i as u32 * 20);
            let actual_index = self.scroll_offset + i;
            
            // Highlight selected
            if Some(actual_index) == self.selected_index {
                self.window.fill_rect(0, y as i32, w as i32, 20, Color::CYAN.to_u32());
            }
            
            // Draw icon
            let icon = if entry.is_dir { "[DIR]" } else { "[FILE]" };
            self.draw_text(icon, 5, y as i32 + 5, Color::BLACK);
            
            // Draw name
            self.draw_text(&entry.name, 60, y as i32 + 5, Color::BLACK);
            
            // Draw size for files
            if !entry.is_dir {
                let size_str = format!("{} B", entry.size);
                self.draw_text(&size_str, (w - 100) as i32, y as i32 + 5, Color::DARK_GRAY);
            }
        }
        
        // Draw status bar
        self.window.fill_rect(0, (h as i32) - 30, w as i32, 30, Color::DARK_GRAY.to_u32());
        let status_msg = self.status_msg.clone();
        self.draw_text(&status_msg, 5, (h - 22) as i32, Color::WHITE);
        
        // Draw button hints (compact to fit 320px width)
        let hint = "Ent=Open Bksp=Up";
        let hint_w = hint.len() as i32 * libpickleos::font::GLYPH_W;
        self.draw_text(hint, (w as i32 - hint_w - 6).max(0), (h - 22) as i32, Color::LIGHT_GRAY);
        
        self.window.commit();
    }
    
    fn draw_text(&mut self, text: &str, x: i32, y: i32, color: Color) {
        libpickleos::font::draw_string(&mut self.window, text, x, y, color.to_u32());
    }
    
    fn handle_event(&mut self, event: Event) -> Result<bool, &'static str> {
        match event {
            Event::Key(key) => {
                match key {
                    b'\n' | b'\r' => {
                        // Enter - open selected item
                        if let Some(idx) = self.selected_index {
                            if idx < self.entries.len() {
                                let entry = self.entries[idx].clone();
                                if entry.is_dir {
                                    self.navigate_to(&entry.name)?;
                                } else {
                                    self.status_msg = format!("Cannot open file: {}", entry.name);
                                }
                            }
                        }
                    }
                    b'\x08' => {
                        // Backspace - go up
                        self.navigate_to("..")?;
                    }
                    b'j' | b'J' => {
                        // Down
                        if let Some(idx) = self.selected_index {
                            if idx + 1 < self.entries.len() {
                                self.selected_index = Some(idx + 1);
                            }
                        } else if !self.entries.is_empty() {
                            self.selected_index = Some(0);
                        }
                    }
                    b'k' | b'K' => {
                        // Up
                        if let Some(idx) = self.selected_index {
                            if idx > 0 {
                                self.selected_index = Some(idx - 1);
                            }
                        }
                    }
                    b'd' | b'D' => {
                        // Delete selected item
                        if let Some(idx) = self.selected_index {
                            if idx < self.entries.len() {
                                let entry = self.entries[idx].clone();
                                if entry.name != ".." {
                                    self.delete_item(&entry)?;
                                }
                            }
                        }
                    }
                    b'n' | b'N' => {
                        // Create new directory (simplified - hardcoded name)
                        let new_dir = format!("{}/newdir", self.current_path.trim_end_matches('/'));
                        if syscall::sys_mkdir(&new_dir) == 0 {
                            self.status_msg = String::from("Created newdir");
                            self.load_directory()?;
                        } else {
                            self.status_msg = String::from("Failed to create directory");
                        }
                    }
                    b'q' | b'Q' => {
                        // Quit
                        return Ok(true);
                    }
                    _ => {}
                }
            }
            Event::MouseDown { x: _, y } => {
                // Click on file list
                let h = self.window.size().1 as i32;
                if y >= 30 && y < h - 30 {
                    let item_index = ((y - 30) / 20) as usize + self.scroll_offset;
                    if item_index < self.entries.len() {
                        self.selected_index = Some(item_index);
                    }
                }
            }
            Event::Close => {
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    }
    
    fn navigate_to(&mut self, target: &str) -> Result<(), &'static str> {
        let new_path = if target == ".." {
            // Go up one level
            if self.current_path == "/" {
                return Ok(());
            }
            let mut parts: Vec<&str> = self.current_path.split('/').filter(|s| !s.is_empty()).collect();
            if !parts.is_empty() {
                parts.pop();
            }
            if parts.is_empty() {
                String::from("/")
            } else {
                format!("/{}", parts.join("/"))
            }
        } else {
            // Navigate into subdirectory
            if self.current_path == "/" {
                format!("/{}", target)
            } else {
                format!("{}/{}", self.current_path, target)
            }
        };
        
        self.current_path = new_path;
        self.load_directory()?;
        Ok(())
    }
    
    fn delete_item(&mut self, entry: &FileEntry) -> Result<(), &'static str> {
        let mut path = self.current_path.clone();
        if !path.ends_with('/') {
            path.push('/');
        }
        path.push_str(&entry.name);
        
        let result = if entry.is_dir {
            syscall::sys_rmdir(&path)
        } else {
            syscall::sys_unlink(&path)
        };
        
        if result == 0 {
            self.status_msg = format!("Deleted: {}", entry.name);
            self.load_directory()?;
        } else {
            self.status_msg = format!("Failed to delete: {}", entry.name);
        }
        
        Ok(())
    }
    
    fn run(&mut self) -> Result<(), &'static str> {
        loop {
            self.draw();
            
            // Poll for events
            if let Some(event) = self.window.poll_event() {
                if self.handle_event(event)? {
                    break;
                }
            }
            
            // Small delay
            syscall::sys_yield();
        }
        
        Ok(())
    }
}

fn run_file_manager() -> Result<(), &'static str> {
    let mut fm = FileManager::new()?;
    fm.run()
}
