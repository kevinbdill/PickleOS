// Pipe / IPC test for PICKLE OS.
//
// Exercises the new SYS_PIPE syscall together with fork/wait/exit:
//   1. The parent creates a pipe (read_fd, write_fd).
//   2. It forks. Both processes inherit copies of both pipe ends.
//   3. The child closes the write end, reads the message from the pipe until
//      EOF, prints it back, and exits.
//   4. The parent closes the read end, writes a message into the pipe, closes
//      the write end (which signals EOF to the child), then waits for the
//      child and reports its exit status.
//
// All output goes through SYS_PRINT so it appears on the serial console.

// Syscall numbers (must match the kernel's syscall.rs).
#define SYS_PRINT   1
#define SYS_GETPID  2
#define SYS_READ   14
#define SYS_WRITE  15
#define SYS_CLOSE  16
#define SYS_EXIT2  25
#define SYS_WAIT   26
#define SYS_FORK   28
#define SYS_PIPE   29

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

static inline long sys_print(const char *msg, unsigned long len) {
    return syscall3(SYS_PRINT, (long)msg, (long)len, 0);
}

static inline long sys_getpid(void) {
    return syscall3(SYS_GETPID, 0, 0, 0);
}

static inline long sys_fork(void) {
    return syscall3(SYS_FORK, 0, 0, 0);
}

static inline long sys_wait(int *status) {
    return syscall3(SYS_WAIT, (long)status, 0, 0);
}

static inline long sys_pipe(unsigned int fds[2]) {
    return syscall3(SYS_PIPE, (long)fds, 0, 0);
}

static inline long sys_read(int fd, void *buf, unsigned long n) {
    return syscall3(SYS_READ, (long)fd, (long)buf, (long)n);
}

static inline long sys_write(int fd, const void *buf, unsigned long n) {
    return syscall3(SYS_WRITE, (long)fd, (long)buf, (long)n);
}

static inline long sys_close(int fd) {
    return syscall3(SYS_CLOSE, (long)fd, 0, 0);
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

static void print_int(long v) {
    char buf[24];
    int i = 0;
    int neg = 0;
    unsigned long u;

    if (v < 0) {
        neg = 1;
        u = (unsigned long)(-(v + 1)) + 1UL;
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

    char out[24];
    int j = 0;
    while (i > 0) {
        out[j++] = buf[--i];
    }
    sys_print(out, (unsigned long)j);
}

// --- entry point -----------------------------------------------------------

void _start(void) {
    print("[pipe_test] starting (pid ");
    print_int(sys_getpid());
    print(")\n");

    unsigned int fds[2] = { 0, 0 };
    if (sys_pipe(fds) != 0) {
        print("[pipe_test] pipe() FAILED\n");
        sys_exit(1);
    }

    int read_fd = (int)fds[0];
    int write_fd = (int)fds[1];

    print("[pipe_test] created pipe: read_fd=");
    print_int(read_fd);
    print(" write_fd=");
    print_int(write_fd);
    print("\n");

    long pid = sys_fork();
    if (pid < 0) {
        print("[pipe_test] fork FAILED\n");
        sys_exit(1);
    }

    if (pid == 0) {
        // ---- Child: read from the pipe ----
        sys_close(write_fd); // child only reads

        char buf[128];
        long total = 0;
        for (;;) {
            long n = sys_read(read_fd, buf + total, sizeof(buf) - 1 - (unsigned long)total);
            if (n <= 0) {
                break; // EOF (0) or error (<0)
            }
            total += n;
            if ((unsigned long)total >= sizeof(buf) - 1) {
                break;
            }
        }
        buf[total] = '\0';

        print("[pipe_test] child received: \"");
        sys_print(buf, (unsigned long)total);
        print("\" (");
        print_int(total);
        print(" bytes)\n");

        sys_close(read_fd);
        sys_exit(0);
    }

    // ---- Parent: write to the pipe ----
    sys_close(read_fd); // parent only writes

    const char *msg = "hello from the parent via pipe!";
    long want = (long)strlen(msg);
    long wrote = sys_write(write_fd, msg, (unsigned long)want);

    print("[pipe_test] parent wrote ");
    print_int(wrote);
    print(" bytes to the pipe\n");

    // Closing the write end signals EOF to the child's reader.
    sys_close(write_fd);

    int status = -1;
    long reaped = sys_wait(&status);
    print("[pipe_test] parent reaped child pid ");
    print_int(reaped);
    print(" with status ");
    print_int((long)status);
    print("\n");

    print("[pipe_test] done\n");
    sys_exit(0);
}
