// argv/envp test for PICKLE OS.
//
// Demonstrates that exec() now passes an argument vector and environment to
// the new process image, laid out on the user stack per the System V x86-64
// ABI (argc, argv[], NULL, envp[], NULL, auxv).
//
//   1. On first launch (init gives us just argv[0]) we print our own argv/envp,
//      then fork a child that exec's /bin/args_test again with a richer argv
//      and a small envp.
//   2. The re-exec'd child prints the argv/envp it received, proving they
//      crossed the exec boundary intact, then exits.
//
// All output goes through SYS_PRINT so it shows up on the serial console.

#define SYS_PRINT   1
#define SYS_GETPID  2
#define SYS_EXIT2  25
#define SYS_WAIT   26
#define SYS_EXEC   27
#define SYS_FORK   28

// 3-argument syscall (rdi, rsi, rdx).
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

// 4-argument syscall: the 4th argument is passed in r10 (matching the kernel's
// SYS_EXEC handler, which reads envp from r10).
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
static inline long sys_fork(void) { return syscall3(SYS_FORK, 0, 0, 0); }
static inline long sys_wait(int *status) { return syscall3(SYS_WAIT, (long)status, 0, 0); }
static inline void sys_exit(int code) { syscall3(SYS_EXIT2, code, 0, 0); __builtin_unreachable(); }
static inline long sys_exec(const char *path, unsigned long len, char **argv, char **envp) {
    return syscall4(SYS_EXEC, (long)path, (long)len, (long)argv, (long)envp);
}

static unsigned long strlen(const char *s) { unsigned long n = 0; while (s[n]) n++; return n; }
static void print(const char *s) { sys_print(s, strlen(s)); }

static void print_int(long v) {
    char buf[24]; int i = 0; int neg = 0; unsigned long u;
    if (v < 0) { neg = 1; u = (unsigned long)(-(v + 1)) + 1UL; } else { u = (unsigned long)v; }
    if (u == 0) buf[i++] = '0';
    else while (u > 0) { buf[i++] = (char)('0' + (u % 10)); u /= 10; }
    if (neg) buf[i++] = '-';
    char out[24]; int j = 0;
    while (i > 0) out[j++] = buf[--i];
    sys_print(out, (unsigned long)j);
}

// Print all entries of a NULL-terminated string vector with a label prefix.
static void dump_vec(const char *label, char **vec) {
    int i = 0;
    while (vec && vec[i]) {
        print(label);
        print("[");
        print_int(i);
        print("] = ");
        print(vec[i]);
        print("\n");
        i++;
    }
}

// The real entry. _start (below) reads argc/argv/envp off the stack and calls
// this with the System V calling convention.
int main(int argc, char **argv, char **envp) {
    print("[args_test] pid ");
    print_int(syscall3(SYS_GETPID, 0, 0, 0));
    print(": argc = ");
    print_int(argc);
    print("\n");

    dump_vec("[args_test] argv", argv);
    dump_vec("[args_test] envp", envp);

    if (argc < 2) {
        // First launch (from init): re-exec ourselves with real args + env.
        print("[args_test] forking child to re-exec with args + env\n");
        long pid = sys_fork();
        if (pid < 0) { print("[args_test] fork FAILED\n"); sys_exit(1); }

        if (pid == 0) {
            static char a0[] = "/bin/args_test";
            static char a1[] = "alpha";
            static char a2[] = "beta";
            static char a3[] = "gamma";
            static char *new_argv[] = { a0, a1, a2, a3, 0 };

            static char e0[] = "HOME=/root";
            static char e1[] = "SHELL=pickle";
            static char *new_envp[] = { e0, e1, 0 };

            sys_exec("/bin/args_test", strlen("/bin/args_test"), new_argv, new_envp);
            print("[args_test] child: exec FAILED\n");
            sys_exit(2);
        }

        int status = -1;
        long reaped = sys_wait(&status);
        print("[args_test] reaped child pid ");
        print_int(reaped);
        print(" status ");
        print_int((long)status);
        print("\n");
    } else {
        print("[args_test] re-exec'd instance received args + env OK\n");
    }

    print("[args_test] done\n");
    sys_exit(0);
}

// Freestanding entry: pull argc/argv/envp off the stack the kernel built and
// hand them to main() in the System V ABI registers (rdi, rsi, rdx).
__asm__(
    ".global _start\n"
    "_start:\n"
    "    mov (%rsp), %rdi\n"             // argc
    "    lea 8(%rsp), %rsi\n"            // argv = &stack[1]
    "    lea 16(%rsp,%rdi,8), %rdx\n"    // envp = &argv[argc+1]
    "    and $-16, %rsp\n"               // 16-byte align before the call
    "    call main\n"
    "    mov %eax, %edi\n"               // exit(main(...))
    "    mov $25, %eax\n"                // SYS_EXIT2
    "    int $0x80\n"
);
