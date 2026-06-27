// Socket-syscall test for PICKLE OS.
//
// Exercises the user-space TCP socket API end-to-end through the kernel
// syscalls added for networking:
//
//   1. SERVER PATH (deterministic, needs no peer):
//        socket() -> bind(port) -> listen() -> shutdown()/close()
//      This verifies the kernel's socket file-descriptor plumbing plus the
//      new bound-port tracking (so listen() picks up the bound port instead
//      of a hard-coded one).
//
//   2. CLIENT PATH (best-effort):
//        socket() -> connect(10.0.2.2:9999) -> send() -> recv()
//      Under QEMU user networking, 10.0.2.2 is the host. If an echo server is
//      listening there the message is echoed back and printed; otherwise the
//      attempt simply times out and is reported without failing the test.
//
// All output goes through SYS_PRINT so it appears on the serial console for
// headless boot verification.
//
// NOTE: the CLIENT PATH is compiled out by default (NET_TEST_CLIENT == 0).
// It polls send()/recv() in a tight loop against a host echo server; under the
// current cooperative net-demo poll model this can contend with the network
// stack lock and is only meaningful with a real peer listening on the host.
// Build with -DNET_TEST_CLIENT=1 and run it manually (with a host echo server
// on 10.0.2.2:9999) to exercise the data path. The default boot run performs
// only the deterministic, peer-free server path so it can never stall init.

#ifndef NET_TEST_CLIENT
#define NET_TEST_CLIENT 0
#endif

#define SYS_PRINT     1
#define SYS_YIELD     4
#define SYS_CLOSE    16
#define SYS_EXIT2    25
#define SYS_SOCKET   39
#define SYS_BIND     40
#define SYS_CONNECT  41
#define SYS_LISTEN   42
#define SYS_ACCEPT   43
#define SYS_SEND     44
#define SYS_RECV     45
#define SYS_SHUTDOWN 46

#define AF_INET      2
#define SOCK_STREAM  1

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
static inline void sys_yield(void) { syscall3(SYS_YIELD, 0, 0, 0); }
static inline long sys_close(int fd) { return syscall3(SYS_CLOSE, fd, 0, 0); }
static inline void sys_exit(int c) { syscall3(SYS_EXIT2, c, 0, 0); __builtin_unreachable(); }
static inline long sys_socket(long dom, long ty, long proto) { return syscall3(SYS_SOCKET, dom, ty, proto); }
static inline long sys_bind(int fd, const void *a, unsigned long l) { return syscall3(SYS_BIND, fd, (long)a, (long)l); }
static inline long sys_listen(int fd, int b) { return syscall3(SYS_LISTEN, fd, b, 0); }
static inline long sys_connect(int fd, const void *a, unsigned long l) { return syscall3(SYS_CONNECT, fd, (long)a, (long)l); }
static inline long sys_send(int fd, const void *b, unsigned long l) { return syscall3(SYS_SEND, fd, (long)b, (long)l); }
static inline long sys_recv(int fd, void *b, unsigned long l) { return syscall3(SYS_RECV, fd, (long)b, (long)l); }
static inline long sys_shutdown(int fd, int how) { return syscall3(SYS_SHUTDOWN, fd, how, 0); }

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

// Build a sockaddr_in: family(2) + port(network order) + 4-byte IPv4 + pad.
static void make_sockaddr(unsigned char *buf, unsigned short port,
                          unsigned char a, unsigned char b,
                          unsigned char c, unsigned char d) {
    for (int i = 0; i < 16; i++) buf[i] = 0;
    buf[0] = AF_INET;        // family low byte
    buf[1] = 0;
    buf[2] = (port >> 8) & 0xFF; // port high (network/big-endian)
    buf[3] = port & 0xFF;        // port low
    buf[4] = a; buf[5] = b; buf[6] = c; buf[7] = d;
}

void _start(void) {
    print("[net_test] starting socket syscall test\n");

    // ---- 1. Server path: socket -> bind -> listen ----
    long sfd = sys_socket(AF_INET, SOCK_STREAM, 0);
    if (sfd < 0) {
        print("[net_test] socket() FAILED (is networking enabled?)\n");
        sys_exit(1);
    }
    print("[net_test] socket() OK, fd ");
    print_int(sfd);
    print("\n");

    unsigned char addr[16];
    make_sockaddr(addr, 8080, 0, 0, 0, 0); // bind 0.0.0.0:8080
    if (sys_bind((int)sfd, addr, 16) != 0) {
        print("[net_test] bind(8080) FAILED\n");
        sys_close((int)sfd);
        sys_exit(1);
    }
    print("[net_test] bind(0.0.0.0:8080) OK\n");

    if (sys_listen((int)sfd, 4) != 0) {
        print("[net_test] listen() FAILED\n");
        sys_close((int)sfd);
        sys_exit(1);
    }
    print("[net_test] listen() OK (bound-port tracking works)\n");

    // Tear the listening socket down again.
    sys_shutdown((int)sfd, 0);
    sys_close((int)sfd);
    print("[net_test] server-path teardown OK\n");

#if NET_TEST_CLIENT
    // ---- 2. Client path: connect to host echo server (best effort) ----
    long cfd = sys_socket(AF_INET, SOCK_STREAM, 0);
    if (cfd < 0) {
        print("[net_test] client socket() FAILED\n");
        sys_exit(1);
    }
    unsigned char raddr[16];
    make_sockaddr(raddr, 9999, 10, 0, 2, 2); // QEMU host = 10.0.2.2:9999
    if (sys_connect((int)cfd, raddr, 16) != 0) {
        print("[net_test] connect() returned error (no host echo server?)\n");
        sys_close((int)cfd);
        print("[net_test] done\n");
        sys_exit(0);
    }
    print("[net_test] connect(10.0.2.2:9999) initiated\n");

    // Send a message, retrying until the connection is established (send
    // returns 0 while not yet connected). Bounded so we never hang at boot.
    const char *msg = "PICKLE\n";
    unsigned long mlen = strlen(msg);
    long sent = 0;
    for (int i = 0; i < 2000 && sent == 0; i++) {
        long n = sys_send((int)cfd, msg, mlen);
        if (n > 0) { sent = n; break; }
        if (n < 0) break;
        sys_yield();
    }
    if (sent <= 0) {
        print("[net_test] no echo server reachable, skipping data test\n");
        sys_close((int)cfd);
        print("[net_test] done\n");
        sys_exit(0);
    }
    print("[net_test] sent ");
    print_int(sent);
    print(" bytes\n");

    // Wait for the echo.
    char rbuf[64];
    long got = 0;
    for (int i = 0; i < 2000; i++) {
        long n = sys_recv((int)cfd, rbuf, sizeof(rbuf) - 1);
        if (n > 0) { got = n; break; }
        if (n < 0) break;
        sys_yield();
    }
    if (got > 0) {
        rbuf[got] = 0;
        print("[net_test] received echo: ");
        sys_print(rbuf, (unsigned long)got);
        if (rbuf[got - 1] != '\n') print("\n");
        print("[net_test] ECHO TEST PASSED\n");
    } else {
        print("[net_test] no echo received\n");
    }

    sys_shutdown((int)cfd, 0);
    sys_close((int)cfd);
#else
    print("[net_test] client probe disabled (build -DNET_TEST_CLIENT=1 to enable)\n");
#endif
    print("[net_test] done\n");
    sys_exit(0);
}
