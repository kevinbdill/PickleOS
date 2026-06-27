// Test program using libnextos (to be replaced with Rust version)
// This is a placeholder for testing

#include <stdint.h>
#include <stddef.h>

// Syscalls
#define SYS_PRINT 1
#define SYS_GETPID 2
#define SYS_EXIT 3
#define SYS_TICKS 5
#define SYS_SLEEP 9

static inline uint64_t syscall3(uint64_t num, uint64_t a0, uint64_t a1, uint64_t a2) {
    uint64_t ret;
    __asm__ volatile("int $0x80" : "=a"(ret) : "a"(num), "D"(a0), "S"(a1), "d"(a2) : "rcx", "r11", "memory");
    return ret;
}

static void sys_print(const char *s) {
    size_t len = 0;
    while (s[len]) len++;
    syscall3(SYS_PRINT, (uint64_t)s, len, 0);
}

static uint64_t sys_getpid(void) {
    return syscall3(SYS_GETPID, 0, 0, 0);
}

static uint64_t sys_ticks(void) {
    return syscall3(SYS_TICKS, 0, 0, 0);
}

static void sys_sleep(uint64_t ticks) {
    syscall3(SYS_SLEEP, ticks, 0, 0);
}

static void sys_exit(int code) {
    syscall3(SYS_EXIT, code, 0, 0);
    while(1);
}

void _start(void) {
    sys_print("Test program with new syscalls\n");
    sys_print("My PID: ");
    // Can't easily print numbers without sprintf, so just skip
    sys_print("<pid>\n");
    
    sys_print("Testing SYS_SLEEP...\n");
    uint64_t start = sys_ticks();
    sys_sleep(50); // Sleep for 50 ticks
    uint64_t end = sys_ticks();
    sys_print("Slept for ~50 ticks\n");
    
    sys_print("Test complete. Exiting.\n");
    sys_exit(0);
}
