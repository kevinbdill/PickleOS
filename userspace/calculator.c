/*
 * Simple Calculator GUI Demo for PickleOS
 * 
 * Demonstrates the widget toolkit by creating a functional calculator
 * with number buttons, operators, and display.
 */

// Syscall numbers
#define SYS_EXIT2 25
#define SYS_WIN_CREATE 34
#define SYS_WIN_COMMIT 35
#define SYS_WIN_POLL 36
#define SYS_WIN_DESTROY 37
#define SYS_PRINT 2
#define SYS_YIELD 18

// Event opcodes (matching kernel wm.rs)
#define EV_MOUSE_MOVE 1
#define EV_MOUSE_DOWN 2
#define EV_MOUSE_UP 3
#define EV_KEY 4
#define EV_FOCUS 5
#define EV_BLUR 6
#define EV_CLOSE 7

// Colors
#define COLOR_WHITE 0xFFFFFF
#define COLOR_LIGHTGRAY 0xC8C8C8
#define COLOR_GRAY 0x808080
#define COLOR_DARKGRAY 0x404040
#define COLOR_BLACK 0x000000
#define COLOR_BLUE 0x0080FF
#define COLOR_GREEN 0x00C800
#define COLOR_RED 0xFF0000

// Calculator state
static unsigned int display_value = 0;
static unsigned int stored_value = 0;
static char operation = 0; // '+', '-', '*', '/', '=' or 0 for none
static int new_number = 1; // flag: next digit starts a new number

// Window dimensions
#define WIN_WIDTH 240
#define WIN_HEIGHT 280
static unsigned long window_id = 0;
static unsigned int framebuffer[WIN_WIDTH * WIN_HEIGHT];

// Syscall wrappers
static inline long syscall3(long num, long arg1, long arg2, long arg3) {
    long ret;
    __asm__ volatile(
        "mov %1, %%rax\n"
        "mov %2, %%rdi\n"
        "mov %3, %%rsi\n"
        "mov %4, %%rdx\n"
        "int $0x80\n"
        "mov %%rax, %0\n"
        : "=r"(ret)
        : "r"(num), "r"(arg1), "r"(arg2), "r"(arg3)
        : "rax", "rdi", "rsi", "rdx", "memory"
    );
    return ret;
}

static inline long syscall4(long num, long arg1, long arg2, long arg3, long arg4) {
    long ret;
    __asm__ volatile(
        "mov %1, %%rax\n"
        "mov %2, %%rdi\n"
        "mov %3, %%rsi\n"
        "mov %4, %%rdx\n"
        "mov %5, %%r10\n"
        "int $0x80\n"
        "mov %%rax, %0\n"
        : "=r"(ret)
        : "r"(num), "r"(arg1), "r"(arg2), "r"(arg3), "r"(arg4)
        : "rax", "rdi", "rsi", "rdx", "r10", "memory"
    );
    return ret;
}

static void sys_print(const char* str) {
    long len = 0;
    while (str[len]) len++;
    syscall3(SYS_PRINT, (long)str, len, 0);
}

static void sys_exit(int code) {
    syscall3(SYS_EXIT2, code, 0, 0);
    while(1);
}

static void sys_yield(void) {
    syscall3(SYS_YIELD, 0, 0, 0);
}

// Window management
static unsigned long sys_win_create(unsigned int w, unsigned int h, const char* title) {
    long len = 0;
    while (title[len]) len++;
    return syscall4(SYS_WIN_CREATE, w, h, (long)title, len);
}

static void sys_win_commit(unsigned long win_id, const unsigned int* pixels) {
    syscall3(SYS_WIN_COMMIT, win_id, (long)pixels, 0);
}

static int sys_win_poll(unsigned long win_id, unsigned char* event_buf) {
    return (int)syscall3(SYS_WIN_POLL, win_id, (long)event_buf, 0);
}

// Drawing helpers
static void fill_rect(unsigned int x, unsigned int y, unsigned int w, unsigned int h, unsigned int color) {
    for (unsigned int dy = 0; dy < h; dy++) {
        for (unsigned int dx = 0; dx < w; dx++) {
            unsigned int px = x + dx;
            unsigned int py = y + dy;
            if (px < WIN_WIDTH && py < WIN_HEIGHT) {
                framebuffer[py * WIN_WIDTH + px] = color;
            }
        }
    }
}

static void draw_char(unsigned int x, unsigned int y, char ch, unsigned int fg) {
    // Simple 5x7 pixel character rendering
    // For simplicity, just draw a small block representing the character
    for (int dy = 0; dy < 7; dy++) {
        for (int dx = 0; dx < 5; dx++) {
            framebuffer[(y + dy) * WIN_WIDTH + (x + dx)] = fg;
        }
    }
}

static void draw_text(unsigned int x, unsigned int y, const char* text, unsigned int fg) {
    int offset = 0;
    while (*text) {
        draw_char(x + offset * 6, y, *text, fg);
        text++;
        offset++;
    }
}

static void draw_button(unsigned int x, unsigned int y, unsigned int w, unsigned int h, 
                       const char* label, int pressed) {
    unsigned int bg = pressed ? COLOR_DARKGRAY : COLOR_GRAY;
    unsigned int border = pressed ? COLOR_BLACK : COLOR_WHITE;
    unsigned int shadow = pressed ? COLOR_LIGHTGRAY : COLOR_BLACK;
    
    // Background
    fill_rect(x, y, w, h, bg);
    
    // Top and left borders
    for (unsigned int i = 0; i < w; i++) {
        framebuffer[y * WIN_WIDTH + (x + i)] = border;
    }
    for (unsigned int i = 0; i < h; i++) {
        framebuffer[(y + i) * WIN_WIDTH + x] = border;
    }
    
    // Bottom and right borders
    for (unsigned int i = 0; i < w; i++) {
        framebuffer[(y + h - 1) * WIN_WIDTH + (x + i)] = shadow;
    }
    for (unsigned int i = 0; i < h; i++) {
        framebuffer[(y + i) * WIN_WIDTH + (x + w - 1)] = shadow;
    }
    
    // Label
    int len = 0;
    while (label[len]) len++;
    unsigned int text_x = x + (w / 2) - (len * 3);
    unsigned int text_y = y + (h / 2) - 3;
    draw_text(text_x, text_y, label, COLOR_BLACK);
}

// Convert number to string
static void uint_to_str(unsigned int num, char* buf) {
    if (num == 0) {
        buf[0] = '0';
        buf[1] = 0;
        return;
    }
    
    char temp[16];
    int i = 0;
    while (num > 0) {
        temp[i++] = '0' + (num % 10);
        num /= 10;
    }
    
    // Reverse
    for (int j = 0; j < i; j++) {
        buf[j] = temp[i - 1 - j];
    }
    buf[i] = 0;
}

// Calculator logic
static void perform_operation(void) {
    switch (operation) {
        case '+':
            display_value = stored_value + display_value;
            break;
        case '-':
            display_value = stored_value - display_value;
            break;
        case '*':
            display_value = stored_value * display_value;
            break;
        case '/':
            if (display_value != 0) {
                display_value = stored_value / display_value;
            }
            break;
    }
    operation = 0;
}

static void handle_digit(int digit) {
    if (new_number) {
        display_value = digit;
        new_number = 0;
    } else {
        display_value = display_value * 10 + digit;
    }
}

static void handle_operator(char op) {
    if (operation != 0) {
        perform_operation();
    }
    stored_value = display_value;
    operation = op;
    new_number = 1;
}

static void handle_equals(void) {
    if (operation != 0) {
        perform_operation();
        new_number = 1;
    }
}

static void handle_clear(void) {
    display_value = 0;
    stored_value = 0;
    operation = 0;
    new_number = 1;
}

// UI rendering
static void render_display(void) {
    // Display area at top
    fill_rect(10, 10, 220, 30, COLOR_WHITE);
    
    // Border
    for (unsigned int i = 0; i < 220; i++) {
        framebuffer[10 * WIN_WIDTH + (10 + i)] = COLOR_BLACK;
        framebuffer[39 * WIN_WIDTH + (10 + i)] = COLOR_BLACK;
    }
    for (unsigned int i = 0; i < 30; i++) {
        framebuffer[(10 + i) * WIN_WIDTH + 10] = COLOR_BLACK;
        framebuffer[(10 + i) * WIN_WIDTH + 229] = COLOR_BLACK;
    }
    
    // Display value
    char buf[16];
    uint_to_str(display_value, buf);
    draw_text(200, 18, buf, COLOR_BLACK);
}

static void render_buttons(int pressed_btn) {
    // Button layout: 4x5 grid
    // Row 0: 7 8 9 /
    // Row 1: 4 5 6 *
    // Row 2: 1 2 3 -
    // Row 3: 0 C = +
    
    const char* labels[4][4] = {
        {"7", "8", "9", "/"},
        {"4", "5", "6", "*"},
        {"1", "2", "3", "-"},
        {"0", "C", "=", "+"}
    };
    
    for (int row = 0; row < 4; row++) {
        for (int col = 0; col < 4; col++) {
            unsigned int x = 10 + col * 55;
            unsigned int y = 50 + row * 55;
            int btn_id = row * 4 + col;
            draw_button(x, y, 50, 50, labels[row][col], pressed_btn == btn_id);
        }
    }
}

static int get_button_at(int mx, int my) {
    // Check if click is in button area
    if (my < 50 || my >= 270) return -1;
    if (mx < 10 || mx >= 230) return -1;
    
    int col = (mx - 10) / 55;
    int row = (my - 50) / 55;
    
    if (col < 0 || col >= 4 || row < 0 || row >= 4) return -1;
    
    // Check if within button bounds (50x50 with 5px gaps)
    int local_x = (mx - 10) % 55;
    int local_y = (my - 50) % 55;
    if (local_x >= 50 || local_y >= 50) return -1;
    
    return row * 4 + col;
}

static void handle_button_press(int btn_id) {
    // Decode button action
    int row = btn_id / 4;
    int col = btn_id % 4;
    
    if (row == 0) {
        if (col == 0) handle_digit(7);
        else if (col == 1) handle_digit(8);
        else if (col == 2) handle_digit(9);
        else handle_operator('/');
    } else if (row == 1) {
        if (col == 0) handle_digit(4);
        else if (col == 1) handle_digit(5);
        else if (col == 2) handle_digit(6);
        else handle_operator('*');
    } else if (row == 2) {
        if (col == 0) handle_digit(1);
        else if (col == 1) handle_digit(2);
        else if (col == 2) handle_digit(3);
        else handle_operator('-');
    } else if (row == 3) {
        if (col == 0) handle_digit(0);
        else if (col == 1) handle_clear();
        else if (col == 2) handle_equals();
        else handle_operator('+');
    }
}

void _start(void) {
    sys_print("Calculator: starting...\n");
    
    // Create window
    window_id = sys_win_create(WIN_WIDTH, WIN_HEIGHT, "Calculator");
    if (window_id == 0xFFFFFFFFFFFFFFFF) {
        sys_print("Calculator: failed to create window\n");
        sys_exit(1);
    }
    
    sys_print("Calculator: window created\n");
    
    int running = 1;
    int pressed_button = -1;
    int mouse_down = 0;
    
    while (running) {
        // Clear to background
        fill_rect(0, 0, WIN_WIDTH, WIN_HEIGHT, COLOR_LIGHTGRAY);
        
        // Render UI
        render_display();
        render_buttons(pressed_button);
        
        // Commit frame
        sys_win_commit(window_id, framebuffer);
        
        // Process events
        unsigned char event[16];
        while (sys_win_poll(window_id, event)) {
            unsigned char kind = event[0];
            
            if (kind == EV_CLOSE) {
                running = 0;
                break;
            } else if (kind == EV_MOUSE_DOWN) {
                unsigned short mx = *(unsigned short*)&event[2];
                unsigned short my = *(unsigned short*)&event[4];
                int btn = get_button_at(mx, my);
                if (btn >= 0) {
                    pressed_button = btn;
                    mouse_down = 1;
                }
            } else if (kind == EV_MOUSE_UP) {
                if (mouse_down && pressed_button >= 0) {
                    unsigned short mx = *(unsigned short*)&event[2];
                    unsigned short my = *(unsigned short*)&event[4];
                    int btn = get_button_at(mx, my);
                    if (btn == pressed_button) {
                        handle_button_press(btn);
                    }
                }
                pressed_button = -1;
                mouse_down = 0;
            }
        }
        
        sys_yield();
    }
    
    sys_print("Calculator: exiting\n");
    sys_exit(0);
}
