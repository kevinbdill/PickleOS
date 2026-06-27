// Signal-handling test for PICKLE OS.
//
// Exercises the new signal syscalls end-to-end:
//   1. Install a custom SIGUSR1 handler, fork a child (which inherits the
//      handler), and have the parent send SIGUSR1 to the child. The child's
//      handler runs, sets a flag, and the child exits cleanly (status 0).
//   2. Fork a second child with the *default* SIGTERM disposition and send it
//      SIGTERM. The default action terminates it, so wait() reports the
//      signal-encoded status 128 + SIGTERM (= 143).
//
// All output goes through SYS_PRINT so it appears on the serial console.

#define SYS_PRINT     1
#define SYS_GETPID    2
#define SYS_YIELD     4
#define SYS_SLEEP     9
#define SYS_EXIT2    25
#define SYS_WAIT     26
#define SYS_FORK     28
#define SYS_KILL     31
#define SYS_SIGNAL   32
#define SYS_SIGRETURN 33

#define SIGUSR1  10
#define SIGTERM  15

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

static inline long sys_print(const char *msg, unsigned long len) { return syscall3(SYS_PRINT, (long)msg, (long)len, 0); }
static inline long sys_getpid(void) { return syscall3(SYS_GETPID, 0, 0, 0); }
static inline void sys_yield(void) { syscall3(SYS_YIELD, 0, 0, 0); }
static inline void sys_sleep(long t) { syscall3(SYS_SLEEP, t, 0, 0); }
static inline long sys_fork(void) { return syscall3(SYS_FORK, 0, 0, 0); }
static inline long sys_wait(int *status) { return syscall3(SYS_WAIT, (long)status, 0, 0); }
static inline void sys_exit(int code) { syscall3(SYS_EXIT2, code, 0, 0); __builtin_unreachable(); }
static inline long sys_kill(long pid, long sig) { return syscall3(SYS_KILL, pid, sig, 0); }
static inline long sys_signal(long sig, long handler, long restorer) { return syscall3(SYS_SIGNAL, sig, handler, restorer); }

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

// Set to 1 by the SIGUSR1 handler. Marked volatile so the wait loop re-reads it.
static volatile int got_sigusr1 = 0;

// User-space SIGUSR1 handler. The kernel enters here with the signal number in
// the first argument; when it returns, control falls through to the restorer
// trampoline (installed via sys_signal) which issues SYS_SIGRETURN.
static void sigusr1_handler(int sig) {
    (void)sig;
    got_sigusr1 = 1;
    print("[signal_test]   >> child caught SIGUSR1 in handler\n");
}

// Signal-return trampoline: the address the kernel pushes as the handler's
// return address. It simply asks the kernel to restore the interrupted context.
__asm__(
    ".global __sigreturn_tramp\n"
    "__sigreturn_tramp:\n"
    "    mov $33, %rax\n"   // SYS_SIGRETURN
    "    int $0x80\n"
);
extern void __sigreturn_tramp(void);

int main(void) {
    print("[signal_test] parent pid ");
    print_int(sys_getpid());
    print("\n");

    // Install the SIGUSR1 handler *before* forking so the child inherits it.
    sys_signal(SIGUSR1, (long)&sigusr1_handler, (long)&__sigreturn_tramp);

    // --- Test 1: caught signal (SIGUSR1) ----------------------------------
    long child = sys_fork();
    if (child < 0) { print("[signal_test] fork FAILED\n"); sys_exit(1); }

    if (child == 0) {
        // Child: spin yielding until the handler sets the flag.
        print("[signal_test] child waiting for SIGUSR1...\n");
        for (int i = 0; i < 100000 && !got_sigusr1; i++) {
            sys_yield();
        }
        if (got_sigusr1) {
            print("[signal_test] child observed handler flag, exiting 0\n");
            sys_exit(0);
        }
        print("[signal_test] child timed out waiting for signal\n");
        sys_exit(1);
    }

    // Parent: give the child a moment to reach its wait loop, then signal it.
    sys_sleep(2);
    print("[signal_test] parent sending SIGUSR1 to child ");
    print_int(child);
    print("\n");
    sys_kill(child, SIGUSR1);

    int status = -1;
    long reaped = sys_wait(&status);
    print("[signal_test] reaped child ");
    print_int(reaped);
    print(" status ");
    print_int((long)status);
    print(" (expected 0)\n");

    // --- Test 2: default action (SIGTERM terminates) ----------------------
    long child2 = sys_fork();
    if (child2 < 0) { print("[signal_test] fork FAILED\n"); sys_exit(1); }

    if (child2 == 0) {
        // No SIGTERM handler installed -> default action will terminate us.
        print("[signal_test] child2 looping, awaiting SIGTERM...\n");
        for (;;) { sys_sleep(1); }
    }

    sys_sleep(2);
    print("[signal_test] parent sending SIGTERM to child2 ");
    print_int(child2);
    print("\n");
    sys_kill(child2, SIGTERM);

    int status2 = -1;
    long reaped2 = sys_wait(&status2);
    print("[signal_test] reaped child2 ");
    print_int(reaped2);
    print(" status ");
    print_int((long)status2);
    print(" (expected 143 = 128+SIGTERM)\n");

    print("[signal_test] done\n");
    sys_exit(0);
}

// Freestanding entry: this program takes no args, so just call main and exit.
__asm__(
    ".global _start\n"
    "_start:\n"
    "    and $-16, %rsp\n"
    "    call main\n"
    "    mov %eax, %edi\n"
    "    mov $25, %eax\n"   // SYS_EXIT2
    "    int $0x80\n"
);
