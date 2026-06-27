// Position-Independent Executable (PIE) test for dynamic linking.
//
// This program tests that the ELF loader can handle ET_DYN binaries with
// relocations (R_X86_64_RELATIVE and potentially symbol-based relocations).
//
// Compiled with -fPIE -pie to generate an ET_DYN binary instead of ET_EXEC.

#define SYS_PRINT     1
#define SYS_GETPID    5
#define SYS_EXIT2    25

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

static inline long sys_print(const char *m, unsigned long l) { return syscall3(SYS_PRINT, (long)m, (long)l, 0); }
static inline long sys_getpid(void) { return syscall3(SYS_GETPID, 0, 0, 0); }
static inline void sys_exit(int c) { syscall3(SYS_EXIT2, c, 0, 0); __builtin_unreachable(); }

static unsigned long strlen(const char *s) { unsigned long n = 0; while (s[n]) n++; return n; }
static void print(const char *s) { sys_print(s, strlen(s)); }
static void print_int(long v) {
    char buf[24]; int i = 0; int neg = 0; unsigned long u;
    if (v < 0) { neg = 1; u = (unsigned long)(-(v + 1)) + 1UL; } else { u = (unsigned long)v; }
    if (u == 0) buf[i++] = '0';
    else while (u > 0) { buf[i++] = (char)('0' + (u % 10)); u /= 10; }
    if (neg) buf[i++] = '-';
    char out[24]; int j = 0; while (i > 0) out[j++] = buf[--i];
    sys_print(out, (unsigned long)j);
}

// Some data that will require relocations
static const char *messages[] = {
    "[pie_test] Starting position-independent executable test\n",
    "[pie_test] PIE binary loaded successfully\n",
    "[pie_test] Relocations applied correctly\n",
    "[pie_test] Process ID: ",
    "\n[pie_test] PIE TEST PASSED\n",
};

// A function pointer that will require a JUMP_SLOT relocation
static void (*test_func)(void) = 0;

static void test_function(void) {
    print(messages[2]); // Test that data relocations work
}

void _start(void) {
    // Test that all relocations work by accessing relocated data
    print(messages[0]);
    print(messages[1]);
    
    // Test function pointer (may generate JUMP_SLOT relocation)
    test_func = test_function;
    if (test_func) {
        test_func();
    }
    
    // Call a syscall to prove we're actually running
    print(messages[3]);
    long pid = sys_getpid();
    print_int(pid);
    print(messages[4]);
    
    sys_exit(0);
}
