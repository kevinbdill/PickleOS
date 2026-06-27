// Minimal user-space hello world program for NextOS
// This program runs in ring 3 and makes syscalls to the kernel.

// Syscall numbers (must match kernel's syscall.rs)
#define SYS_PRINT  1
#define SYS_GETPID 2
#define SYS_EXIT   3

// Inline assembly wrapper for making syscalls via int 0x80
static inline long syscall3(long n, long a1, long a2, long a3) {
    long ret;
    __asm__ volatile(
        "int $0x80"
        : "=a"(ret)
        : "a"(n), "D"(a1), "S"(a2), "d"(a3)
        : "rcx", "r11", "memory"
    );
    return ret;
}

// Syscall wrappers
static inline long sys_print(const char *msg, unsigned long len) {
    return syscall3(SYS_PRINT, (long)msg, (long)len, 0);
}

static inline long sys_getpid(void) {
    return syscall3(SYS_GETPID, 0, 0, 0);
}

static inline void sys_exit(int code) {
    syscall3(SYS_EXIT, code, 0, 0);
    __builtin_unreachable();
}

// String length helper
static unsigned long strlen(const char *s) {
    unsigned long len = 0;
    while (s[len]) len++;
    return len;
}

// Entry point for user programs
void _start(void) {
    const char *msg1 = "[user] Hello from ring 3!\n";
    const char *msg2 = "[user] My PID is: ";
    const char *msg3 = "[user] Syscalls work!\n";
    const char *msg4 = "[user] Exiting...\n";
    
    // Print hello message
    sys_print(msg1, strlen(msg1));
    
    // Get and display PID
    sys_print(msg2, strlen(msg2));
    long pid = sys_getpid();
    
    // Simple number-to-string (just print that we got a PID)
    if (pid >= 0) {
        const char *pid_msg = "(got a PID)\n";
        sys_print(pid_msg, strlen(pid_msg));
    }
    
    // Print success message
    sys_print(msg3, strlen(msg3));
    
    // Print exit message
    sys_print(msg4, strlen(msg4));
    
    // Exit cleanly
    sys_exit(0);
}
