/* Simple test program for standard I/O (stdin/stdout/stderr).
 *
 * This program demonstrates using the file descriptor syscalls (open, read,
 * write, close) to interact with standard streams instead of the kernel's
 * print syscall.
 */

// System call numbers (must match kernel/src/syscall.rs)
#define SYS_WRITE  15
#define SYS_EXIT   2

// Standard file descriptors
#define STDOUT 1
#define STDERR 2

// Inline assembly wrapper for syscalls
static inline long syscall3(long n, long a1, long a2, long a3) {
    long ret;
    __asm__ volatile (
        "int $0x80"
        : "=a"(ret)
        : "a"(n), "D"(a1), "S"(a2), "d"(a3)
        : "rcx", "r11", "memory"
    );
    return ret;
}

// Write to a file descriptor
static long write_fd(int fd, const char *buf, unsigned long count) {
    return syscall3(SYS_WRITE, fd, (long)buf, count);
}

// Calculate string length
static unsigned long strlen(const char *s) {
    unsigned long len = 0;
    while (s[len]) len++;
    return len;
}

// Write string to file descriptor
static void puts_fd(int fd, const char *s) {
    write_fd(fd, s, strlen(s));
}

// Program entry point
void _start(void) {
    // Write to stdout (fd 1)
    puts_fd(STDOUT, "Hello from stdio_test!\n");
    puts_fd(STDOUT, "This message was written via SYS_WRITE to fd 1 (stdout).\n");
    
    // Write to stderr (fd 2)
    puts_fd(STDERR, "[stderr] This is an error message on fd 2.\n");
    
    // Exit cleanly
    syscall3(SYS_EXIT, 0, 0, 0);
    
    // Should never reach here
    while (1);
}
