//! GUI Widget Toolkit for PickleOS
//!
//! This module provides reusable UI components (widgets) for building
//! graphical applications. Widgets handle their own rendering and input
//! events, providing a higher-level abstraction over raw window drawing.

use crate::gui::{Event, Window};
use alloc::string::String;
use alloc::vec::Vec;
use alloc::boxed::Box;

/// RGB color representation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
    
    pub fn to_u32(self) -> u32 {
        ((self.r as u32) << 16) | ((self.g as u32) << 8) | (self.b as u32)
    }
    
    // Standard colors
    pub const BLACK: Color = Color::rgb(0, 0, 0);
    pub const WHITE: Color = Color::rgb(255, 255, 255);
    pub const GRAY: Color = Color::rgb(128, 128, 128);
    pub const LIGHT_GRAY: Color = Color::rgb(200, 200, 200);
    pub const DARK_GRAY: Color = Color::rgb(64, 64, 64);
    pub const RED: Color = Color::rgb(255, 0, 0);
    pub const GREEN: Color = Color::rgb(0, 255, 0);
    pub const BLUE: Color = Color::rgb(0, 128, 255);
    pub const CYAN: Color = Color::rgb(0, 200, 200);
    pub const YELLOW: Color = Color::rgb(255, 255, 0);
}

/// Rectangle representing a region
#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }
    
    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w as i32 &&
        py >= self.y && py < self.y + self.h as i32
    }
}

/// Trait for all widgets
pub trait Widget {
    /// Draw the widget to a window's buffer
    fn draw(&self, win: &mut Window);
    
    /// Handle an input event, return true if handled
    fn handle_event(&mut self, event: &Event) -> bool;
    
    /// Get the widget's bounding rectangle
    fn bounds(&self) -> Rect;
    
    /// Set the widget's position
    fn set_position(&mut self, x: i32, y: i32);
}

/// A clickable button widget
pub struct Button {
    rect: Rect,
    label: String,
    pressed: bool,
    hovered: bool,
    on_click: Option<Box<dyn FnMut()>>,
}

impl Button {
    pub fn new(x: i32, y: i32, w: u32, h: u32, label: &str) -> Self {
        Self {
            rect: Rect::new(x, y, w, h),
            label: String::from(label),
            pressed: false,
            hovered: false,
            on_click: None,
        }
    }
    
    pub fn on_click<F>(mut self, callback: F) -> Self 
    where
        F: FnMut() + 'static,
    {
        self.on_click = Some(Box::new(callback));
        self
    }
    
    fn draw_text(&self, win: &mut Window, text: &str, x: i32, y: i32, fg: Color) {
        // Simple 8x8 bitmap font rendering
        let fg_u32 = fg.to_u32();
        for (i, ch) in text.chars().enumerate() {
            let char_x = x + (i as i32 * 8);
            if ch.is_ascii_graphic() || ch == ' ' {
                // Draw a simple placeholder for the character
                // In a real implementation, you'd use a proper bitmap font
                if ch != ' ' {
                    for dy in 0..8 {
                        for dx in 0..6 {
                            win.put_pixel(char_x + dx, y + dy, fg_u32);
                        }
                    }
                }
            }
        }
    }
}

impl Widget for Button {
    fn draw(&self, win: &mut Window) {
        let bg_color = if self.pressed {
            Color::DARK_GRAY
        } else if self.hovered {
            Color::LIGHT_GRAY
        } else {
            Color::GRAY
        };
        
        // Draw button background
        win.fill_rect(self.rect.x, self.rect.y, self.rect.w as i32, self.rect.h as i32,
            bg_color.to_u32(),
        );
        
        // Draw border
        let border_color = if self.pressed {
            Color::BLACK
        } else {
            Color::WHITE
        };
        
        // Top and left borders (highlight)
        for i in 0..self.rect.w {
            win.put_pixel(self.rect.x + i as i32, self.rect.y, border_color.to_u32());
        }
        for i in 0..self.rect.h {
            win.put_pixel(self.rect.x, self.rect.y + i as i32, border_color.to_u32());
        }
        
        // Bottom and right borders (shadow)
        let shadow_color = if self.pressed { Color::LIGHT_GRAY } else { Color::BLACK };
        for i in 0..self.rect.w {
            win.put_pixel(
                self.rect.x + i as i32, self.rect.y + self.rect.h as i32 - 1,
                shadow_color.to_u32(),
            );
        }
        for i in 0..self.rect.h {
            win.put_pixel(
                self.rect.x + self.rect.w as i32 - 1, self.rect.y + i as i32,
                shadow_color.to_u32(),
            );
        }
        
        // Draw label centered
        let text_x = self.rect.x + (self.rect.w as i32 / 2) - ((self.label.len() * 4) as i32);
        let text_y = self.rect.y + (self.rect.h as i32 / 2) - 4;
        self.draw_text(win, &self.label, text_x, text_y, Color::BLACK);
    }
    
    fn handle_event(&mut self, event: &Event) -> bool {
        match event {
            Event::MouseMove { x, y } => {
                let was_hovered = self.hovered;
                self.hovered = self.rect.contains(*x as i32, *y as i32);
                was_hovered != self.hovered
            }
            Event::MouseDown { x, y } => {
                if self.rect.contains(*x as i32, *y as i32) {
                    self.pressed = true;
                    true
                } else {
                    false
                }
            }
            Event::MouseUp { x, y } => {
                if self.pressed && self.rect.contains(*x as i32, *y as i32) {
                    self.pressed = false;
                    if let Some(ref mut callback) = self.on_click {
                        callback();
                    }
                    true
                } else {
                    self.pressed = false;
                    false
                }
            }
            _ => false,
        }
    }
    
    fn bounds(&self) -> Rect {
        self.rect
    }
    
    fn set_position(&mut self, x: i32, y: i32) {
        self.rect.x = x;
        self.rect.y = y;
    }
}

/// A text label widget
pub struct Label {
    rect: Rect,
    text: String,
    fg_color: Color,
    bg_color: Option<Color>,
}

impl Label {
    pub fn new(x: i32, y: i32, text: &str) -> Self {
        let w = (text.len() * 8) as u32;
        Self {
            rect: Rect::new(x, y, w, 16),
            text: String::from(text),
            fg_color: Color::BLACK,
            bg_color: None,
        }
    }
    
    pub fn with_colors(mut self, fg: Color, bg: Option<Color>) -> Self {
        self.fg_color = fg;
        self.bg_color = bg;
        self
    }
    
    pub fn set_text(&mut self, text: &str) {
        self.text = String::from(text);
        self.rect.w = (text.len() * 8) as u32;
    }
}

impl Widget for Label {
    fn draw(&self, win: &mut Window) {
        // Draw background if specified
        if let Some(bg) = self.bg_color {
            win.fill_rect(self.rect.x, self.rect.y, self.rect.w as i32, self.rect.h as i32,
                bg.to_u32(),
            );
        }
        
        // Draw text (simplified - using pixel blocks)
        let fg_u32 = self.fg_color.to_u32();
        for (i, ch) in self.text.chars().enumerate() {
            let char_x = self.rect.x + (i as i32 * 8);
            if ch.is_ascii_graphic() || ch == ' ' {
                if ch != ' ' {
                    for dy in 0..8 {
                        for dx in 0..6 {
                            win.put_pixel(
                                char_x + dx,
                                self.rect.y + dy + 4,
                                fg_u32,
                            );
                        }
                    }
                }
            }
        }
    }
    
    fn handle_event(&mut self, _event: &Event) -> bool {
        false // Labels don't handle events
    }
    
    fn bounds(&self) -> Rect {
        self.rect
    }
    
    fn set_position(&mut self, x: i32, y: i32) {
        self.rect.x = x;
        self.rect.y = y;
    }
}

/// A text input box widget
pub struct TextBox {
    rect: Rect,
    text: String,
    cursor_pos: usize,
    focused: bool,
    max_length: usize,
}

impl TextBox {
    pub fn new(x: i32, y: i32, w: u32) -> Self {
        Self {
            rect: Rect::new(x, y, w, 24),
            text: String::new(),
            cursor_pos: 0,
            focused: false,
            max_length: 64,
        }
    }
    
    pub fn text(&self) -> &str {
        &self.text
    }
    
    pub fn set_text(&mut self, text: &str) {
        self.text = String::from(text);
        self.cursor_pos = self.text.len();
    }
    
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor_pos = 0;
    }
}

impl Widget for TextBox {
    fn draw(&self, win: &mut Window) {
        // Draw background
        let bg_color = if self.focused {
            Color::WHITE
        } else {
            Color::LIGHT_GRAY
        };
        win.fill_rect(self.rect.x, self.rect.y, self.rect.w as i32, self.rect.h as i32,
            bg_color.to_u32(),
        );
        
        // Draw border
        let border_color = if self.focused { Color::BLUE } else { Color::DARK_GRAY };
        // Top
        for i in 0..self.rect.w {
            win.put_pixel(self.rect.x + i as i32, self.rect.y, border_color.to_u32());
        }
        // Bottom
        for i in 0..self.rect.w {
            win.put_pixel(
                self.rect.x + i as i32, self.rect.y + self.rect.h as i32 - 1,
                border_color.to_u32(),
            );
        }
        // Left
        for i in 0..self.rect.h {
            win.put_pixel(self.rect.x, self.rect.y + i as i32, border_color.to_u32());
        }
        // Right
        for i in 0..self.rect.h {
            win.put_pixel(
                self.rect.x + self.rect.w as i32 - 1, self.rect.y + i as i32,
                border_color.to_u32(),
            );
        }
        
        // Draw text
        let fg_u32 = Color::BLACK.to_u32();
        for (i, ch) in self.text.chars().enumerate() {
            let char_x = self.rect.x + 4 + (i as i32 * 8);
            if ch.is_ascii_graphic() || ch == ' ' {
                if ch != ' ' {
                    for dy in 0..8 {
                        for dx in 0..6 {
                            win.put_pixel(
                                char_x + dx,
                                self.rect.y + 8 + dy,
                                fg_u32,
                            );
                        }
                    }
                }
            }
        }
        
        // Draw cursor if focused
        if self.focused {
            let cursor_x = self.rect.x + 4 + (self.cursor_pos as i32 * 8);
            for i in 0..12 {
                win.put_pixel(cursor_x, self.rect.y + 6 + i, Color::BLACK.to_u32());
            }
        }
    }
    
    fn handle_event(&mut self, event: &Event) -> bool {
        match event {
            Event::MouseDown { x, y } => {
                let was_focused = self.focused;
                self.focused = self.rect.contains(*x as i32, *y as i32);
                was_focused != self.focused
            }
            Event::Key(key) => {
                if !self.focused {
                    return false;
                }
                
                match *key {
                    b'\x08' => {
                        // Backspace
                        if self.cursor_pos > 0 {
                            self.text.remove(self.cursor_pos - 1);
                            self.cursor_pos -= 1;
                        }
                        true
                    }
                    b'\x7F' => {
                        // Delete
                        if self.cursor_pos < self.text.len() {
                            self.text.remove(self.cursor_pos);
                        }
                        true
                    }
                    b'\n' | b'\r' => {
                        // Enter - could trigger a callback
                        true
                    }
                    ch if ch.is_ascii_graphic() || ch == b' ' => {
                        if self.text.len() < self.max_length {
                            self.text.insert(self.cursor_pos, ch as char);
                            self.cursor_pos += 1;
                        }
                        true
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }
    
    fn bounds(&self) -> Rect {
        self.rect
    }
    
    fn set_position(&mut self, x: i32, y: i32) {
        self.rect.x = x;
        self.rect.y = y;
    }
}

/// A container that manages multiple widgets
pub struct Panel {
    rect: Rect,
    widgets: Vec<Box<dyn Widget>>,
    bg_color: Color,
}

impl Panel {
    pub fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self {
            rect: Rect::new(x, y, w, h),
            widgets: Vec::new(),
            bg_color: Color::LIGHT_GRAY,
        }
    }
    
    pub fn add_widget(&mut self, widget: Box<dyn Widget>) {
        self.widgets.push(widget);
    }
    
    pub fn set_bg_color(&mut self, color: Color) {
        self.bg_color = color;
    }
    
    pub fn draw_all(&self, win: &mut Window) {
        // Draw panel background
        win.fill_rect(self.rect.x, self.rect.y, self.rect.w as i32, self.rect.h as i32,
            self.bg_color.to_u32(),
        );
        
        // Draw all widgets
        for widget in &self.widgets {
            widget.draw(win);
        }
    }
    
    pub fn handle_event(&mut self, event: &Event) -> bool {
        // Process events in reverse order (top-most widgets first)
        for widget in self.widgets.iter_mut().rev() {
            if widget.handle_event(event) {
                return true;
            }
        }
        false
    }
}

impl Widget for Panel {
    fn draw(&self, win: &mut Window) {
        self.draw_all(win);
    }
    
    fn handle_event(&mut self, event: &Event) -> bool {
        Panel::handle_event(self, event)
    }
    
    fn bounds(&self) -> Rect {
        self.rect
    }
    
    fn set_position(&mut self, x: i32, y: i32) {
        let dx = x - self.rect.x;
        let dy = y - self.rect.y;
        self.rect.x = x;
        self.rect.y = y;
        
        // Move all child widgets
        for widget in &mut self.widgets {
            let bounds = widget.bounds();
            widget.set_position(bounds.x + dx, bounds.y + dy);
        }
    }
}
