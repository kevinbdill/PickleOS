// getpid / getppid test for PICKLE OS.
//
// Demonstrates the process-identity syscalls:
//   1. The parent prints its own PID and PPID.
//   2. It forks a child; the child prints its PID and PPID and verifies that
//      its PPID matches the parent's PID.
//   3. The parent reaps the child and exits.
//
// All output goes through SYS_PRINT so it appears on the serial console.

#define SYS_PRINT    1
#define SYS_GETPID   2
#define SYS_EXIT2   25
#define SYS_WAIT    26
#define SYS_FORK    28
#define SYS_GETPPID 30

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
static inline long sys_getppid(void) { return syscall3(SYS_GETPPID, 0, 0, 0); }
static inline long sys_fork(void) { return syscall3(SYS_FORK, 0, 0, 0); }
static inline long sys_wait(int *status) { return syscall3(SYS_WAIT, (long)status, 0, 0); }
static inline void sys_exit(int code) { syscall3(SYS_EXIT2, code, 0, 0); __builtin_unreachable(); }

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

void _start(void) {
    long ppid = sys_getppid();
    long pid = sys_getpid();

    print("[pid_test] parent pid ");
    print_int(pid);
    print(", ppid ");
    print_int(ppid);
    print("\n");

    long child = sys_fork();
    if (child < 0) { print("[pid_test] fork FAILED\n"); sys_exit(1); }

    if (child == 0) {
        long cpid = sys_getpid();
        long cppid = sys_getppid();
        print("[pid_test] child pid ");
        print_int(cpid);
        print(", ppid ");
        print_int(cppid);
        print("\n");
        if (cppid == pid) {
            print("[pid_test] child: ppid matches parent pid OK\n");
        } else {
            print("[pid_test] child: ppid MISMATCH!\n");
        }
        sys_exit(0);
    }

    int status = -1;
    long reaped = sys_wait(&status);
    print("[pid_test] parent reaped child ");
    print_int(reaped);
    print(" status ");
    print_int((long)status);
    print("\n");

    print("[pid_test] done\n");
    sys_exit(0);
}
