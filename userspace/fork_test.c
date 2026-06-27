// Process-management test for PICKLE OS.
//
// Exercises the new fork/exec/wait/exit syscalls end-to-end:
//   1. The parent forks a child.
//   2. The child replaces its image with /bin/hello via exec.
//   3. The parent blocks in wait() until the child terminates, then prints
//      the reaped child's PID and exit status.
//
// All output goes through SYS_PRINT so it appears on the serial console.

// Syscall numbers (must match kernel's syscall.rs).
#define SYS_PRINT   1
#define SYS_GETPID  2
#define SYS_EXIT    3   // legacy alias of SYS_EXIT2
#define SYS_EXIT2  25
#define SYS_WAIT   26
#define SYS_EXEC   27
#define SYS_FORK   28

// Inline assembly wrapper for making syscalls via int 0x80.
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

// 4-argument syscall: the 4th argument is passed in r10. SYS_EXEC reads envp
// from r10, so wrappers that call exec must zero/initialize it explicitly
// (a bare syscall3 would leave r10 holding garbage).
static inline long syscall4(long n, long a1, long a2, long a3, long a4) {
    long ret;
    register long r10 __asm__("r10") = a4;
    __asm__ volatile(
        "int $0x80"
        : "=a"(ret)
        : "a"(n), "D"(a1), "S"(a2), "d"(a3), "r"(r10)
        : "rcx", "r11", "memory"
    );
    return ret;
}

static inline long sys_print(const char *msg, unsigned long len) {
    return syscall3(SYS_PRINT, (long)msg, (long)len, 0);
}

static inline long sys_getpid(void) {
    return syscall3(SYS_GETPID, 0, 0, 0);
}

static inline long sys_fork(void) {
    return syscall3(SYS_FORK, 0, 0, 0);
}

static inline long sys_exec(const char *path, unsigned long len) {
    // argv = NULL, envp = NULL (kernel defaults argv[0] to the path).
    return syscall4(SYS_EXEC, (long)path, (long)len, 0, 0);
}

static inline long sys_wait(int *status) {
    return syscall3(SYS_WAIT, (long)status, 0, 0);
}

static inline void sys_exit(int code) {
    syscall3(SYS_EXIT2, code, 0, 0);
    __builtin_unreachable();
}

// --- tiny freestanding helpers --------------------------------------------

static unsigned long strlen(const char *s) {
    unsigned long len = 0;
    while (s[len]) len++;
    return len;
}

static void print(const char *s) {
    sys_print(s, strlen(s));
}

// Print a signed decimal integer followed by nothing (caller adds newline).
static void print_int(long v) {
    char buf[24];
    int i = 0;
    int neg = 0;
    unsigned long u;

    if (v < 0) {
        neg = 1;
        u = (unsigned long)(-(v + 1)) + 1UL; // safe negate (handles LONG_MIN)
    } else {
        u = (unsigned long)v;
    }

    if (u == 0) {
        buf[i++] = '0';
    } else {
        while (u > 0) {
            buf[i++] = (char)('0' + (u % 10));
            u /= 10;
        }
    }
    if (neg) buf[i++] = '-';

    // Reverse into a small output buffer.
    char out[24];
    int j = 0;
    while (i > 0) {
        out[j++] = buf[--i];
    }
    sys_print(out, (unsigned long)j);
}

// --- entry point -----------------------------------------------------------

void _start(void) {
    print("[fork_test] parent starting (pid ");
    print_int(sys_getpid());
    print(")\n");

    long pid = sys_fork();

    if (pid < 0) {
        print("[fork_test] fork FAILED\n");
        sys_exit(1);
    }

    if (pid == 0) {
        // Child path: announce, then replace our image with /bin/hello.
        print("[fork_test] child running (pid ");
        print_int(sys_getpid());
        print("), exec'ing /bin/hello\n");

        const char *path = "/bin/hello";
        sys_exec(path, strlen(path));

        // exec only returns on failure.
        print("[fork_test] child: exec FAILED\n");
        sys_exit(2);
    }

    // Parent path: wait for the child to terminate, then report it.
    print("[fork_test] parent forked child pid ");
    print_int(pid);
    print(", waiting...\n");

    int status = -1;
    long reaped = sys_wait(&status);

    print("[fork_test] parent reaped child pid ");
    print_int(reaped);
    print(" with status ");
    print_int((long)status);
    print("\n");

    print("[fork_test] parent exiting\n");
    sys_exit(0);
}
