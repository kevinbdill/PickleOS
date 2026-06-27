# PickleOS Networking

This document describes the TCP/IP networking implementation in PickleOS.

## Overview

PickleOS now includes a functional TCP/IP networking stack using:
- **E1000 NIC Driver**: Intel 82540EM Gigabit Ethernet controller (emulated by QEMU)
- **smoltcp Stack**: Embedded TCP/IP stack for `no_std` environments
- **Network Services**: ICMP ping responder and TCP echo server

## Architecture

### Components

1. **E1000 Driver** (`kernel/src/driver/e1000.rs`)
   - PCI device discovery and initialization
   - MMIO register access for device control
   - RX/TX descriptor rings with DMA buffers
   - Packet transmission and reception

2. **Network Stack** (`kernel/src/net/stack.rs`)
   - smoltcp integration
   - Device abstraction layer
   - Socket management (TCP/UDP)
   - IP configuration (10.0.2.15/24)

3. **Network Demo Task** (`kernel/src/net/demo.rs`)
   - Continuously polls the network stack
   - Provides basic networking services
   - ICMP echo (ping) is handled automatically by smoltcp
   - TCP echo server on port 7 (ready for implementation)

### Network Configuration

- **IP Address**: 10.0.2.15/24
- **Gateway**: 10.0.2.2
- **MAC Address**: 52:54:00:12:34:56 (read from E1000 EEPROM)
- **MTU**: 1500 bytes

## QEMU Setup

The Makefile includes E1000 emulation with user-mode networking:

```bash
make run          # Headless mode with serial output
make run-display  # Graphical mode
```

QEMU arguments:
```
-device e1000,netdev=net0
-netdev user,id=net0,hostfwd=tcp::5555-:7
```

### Port Forwarding

- Host port 5555 → Guest port 7 (TCP echo server, when implemented)

## Testing

### 1. Ping Test (from host)

Since QEMU user-mode networking doesn't allow direct ping from host to guest,
ping testing requires TAP networking or can be observed via the serial log
showing ICMP packet handling.

### 2. TCP Echo Server (planned)

Once the TCP echo server is fully implemented on port 7:

```bash
# From host machine
telnet localhost 5555
```

Any text sent will be echoed back by the guest OS.

## Socket Syscall API

User-space programs talk to the TCP/IP stack through a small BSD-style socket
syscall surface (invoked via `int $0x80`, syscall number in `rax`, args in
`rdi`/`rsi`/`rdx`). Each socket is exposed to the process as a file descriptor,
so `close(fd)` also tears the socket down.

| #  | Name          | Signature (args)                         | Returns |
|----|---------------|------------------------------------------|---------|
| 39 | `SYS_SOCKET`  | `(domain=AF_INET, type=SOCK_STREAM, 0)`  | fd / -1 |
| 40 | `SYS_BIND`    | `(fd, sockaddr_in*, addrlen)`            | 0 / -1  |
| 41 | `SYS_CONNECT` | `(fd, sockaddr_in*, addrlen)`            | 0 / -1  |
| 42 | `SYS_LISTEN`  | `(fd, backlog)`                          | 0 / -1  |
| 43 | `SYS_ACCEPT`  | `(fd, sockaddr_in* peer\|NULL, 0)`       | fd / -1 |
| 44 | `SYS_SEND`    | `(fd, buf, len, flags)`                  | n / -1  |
| 45 | `SYS_RECV`    | `(fd, buf, len, flags)`                  | n / -1  |
| 46 | `SYS_SHUTDOWN`| `(fd, how)`                              | 0 / -1  |
| 16 | `SYS_CLOSE`   | `(fd)`                                    | 0 / -1  |

`sockaddr_in` is the standard 16-byte layout: family (2 bytes, `AF_INET=2`),
port (2 bytes, **network/big-endian** order), 4-byte IPv4 address, then padding.

### Bound-port tracking

`SYS_BIND` records the requested port in the kernel's `bound_ports` table keyed
by socket id. `SYS_LISTEN` then looks that port up (falling back to port 7 only
if no bind was performed), so a server actually listens on the port it asked
for rather than a hard-coded value.

### Accept model (smoltcp single-socket)

smoltcp's listening socket transitions directly into the connected state once a
peer completes the handshake — there is no separate "accepted" socket object.
`SYS_ACCEPT` therefore waits (cooperatively, yielding) until the listening
socket reports a ready data path (`is_active() && (may_recv() || may_send())`),
optionally fills in the peer's `sockaddr_in`, and returns the **same** fd. The
wait is bounded so a server never spins forever at boot.

### Server example (C)

```c
int fd = sys_socket(AF_INET, SOCK_STREAM, 0);
unsigned char addr[16];
make_sockaddr(addr, 8080, 0, 0, 0, 0);   // 0.0.0.0:8080
sys_bind(fd, addr, 16);
sys_listen(fd, 4);                        // listens on the bound port (8080)

unsigned char peer[16];
int cfd = sys_accept(fd, peer);           // same fd; peer holds the client addr
char buf[64];
long n = sys_recv(cfd, buf, sizeof buf);
sys_send(cfd, buf, n);                     // echo
sys_shutdown(cfd, 0);
sys_close(cfd);
```

### Client example (libpickleos)

```rust
use libpickleos::socket::TcpSocket;

let mut sock = TcpSocket::new()?;
sock.connect([10, 0, 2, 2], 9999)?;       // QEMU host = 10.0.2.2
sock.send(b"PICKLE\n")?;
let mut buf = [0u8; 64];
let n = sock.recv_blocking(&mut buf)?;     // bounded blocking recv
sock.shutdown()?;
// dropping / close() destroys the underlying kernel socket
```

### Reference test program

`userspace/net_test.c` exercises this surface at boot. By default it runs only
the deterministic, peer-free **server path** (`socket → bind(8080) → listen →
shutdown → close`) which verifies the fd plumbing and bound-port tracking
without needing an external peer. The **client path** (connect + send/recv loop
against a host echo server) is compiled out by default — build it with
`-DNET_TEST_CLIENT=1` and run it manually with an echo server listening on
`10.0.2.2:9999` to exercise the live data round-trip.

## Implementation Details

### E1000 Driver

**Initialization Flow**:
1. PCI bus scan to find E1000 device (vendor 0x8086, device 0x100E)
2. Read BAR0 for MMIO base address
3. Read MAC address from EEPROM
4. Allocate DMA buffers for RX/TX descriptor rings
5. Initialize hardware registers (reset, configure RX/TX, enable)
6. Verify link status (link should be up in QEMU)

**RX/TX Rings**:
- RX: 32 descriptors × 2048-byte buffers
- TX: 16 descriptors × 2048-byte buffers
- Descriptor rings allocated from DMA pool
- Buffers managed via physical/virtual address translation

**MMIO Access**:
- Uses `KernelIoGuard` to bypass capability checks during init
- All register access via capability-checked MMIO functions
- Key registers: CTRL, STATUS, RCTL, TCTL, RDT, TDT

### smoltcp Integration

**Device Wrapper** (`E1000Device`):
- Implements smoltcp's `Device` trait
- Provides `RxToken` and `TxToken` for zero-copy packet I/O
- Polls E1000 hardware for received packets
- Transmits packets via E1000 driver

**Network Stack**:
- IPv4 and IPv6 support (proto-ipv4, proto-ipv6)
- TCP and UDP socket infrastructure
- ICMP for ping (handled automatically by smoltcp)
- Ethernet medium
- Default gateway configured (10.0.2.2 for QEMU user networking)

**TCP Echo Server**:
- Single-threaded echo server on port 7
- Handles one connection at a time
- Receives data and echoes it back
- Automatically re-listens after connection closes
- ~100 lines of demonstration code in `net/demo.rs`

## Future Enhancements

### Short Term
1. **TCP Echo Server**: Complete implementation on port 7
2. **DNS Client**: Add DNS resolution support
3. ~~**Socket Syscalls**: Expose socket creation/bind/connect to user-space~~
   — done (see [Socket Syscall API](#socket-syscall-api)): socket/bind/connect/
   listen/accept/send/recv/shutdown/close are all wired through `int $0x80`.

### Medium Term
1. **RTL8139 Driver**: Alternative NIC for broader hardware support
2. **ARP Cache**: Explicit ARP management
3. **Network Statistics**: Packet counters, error rates

### Long Term
1. **WiFi Support**: 802.11 MAC layer, WPA2, driver integration
2. **Firewall**: Packet filtering rules
3. **Routing**: Multi-interface support
4. **IPv6**: Full dual-stack implementation

## Current Limitations

1. **User-Space Access**: Socket syscalls are available (see
   [Socket Syscall API](#socket-syscall-api)). The deterministic server path
   (socket/bind/listen/shutdown/close) is verified in-VM; the live client data
   round-trip depends on QEMU SLIRP delivering RX frames to the E1000 and is
   exercised only via the manual `-DNET_TEST_CLIENT=1` build.
2. **Single Interface**: Only E1000 supported
3. **No WiFi**: Wired Ethernet only
4. **Basic Services**: ICMP responder + socket syscall surface; the TCP echo
   server in `net/demo.rs` drives the stack via cooperative polling.
5. **No Interrupts**: Polling mode (E1000 interrupts not wired). Because the
   stack is polled by the `net-demo` task under a single spin-locked stack,
   user programs that hammer `send()`/`recv()` in a tight loop can contend with
   the poll task — bound such loops and yield between attempts.

## Code Metrics

- **E1000 Driver**: 447 lines (`kernel/src/driver/e1000.rs`)
- **Network Stack**: 317 lines (`kernel/src/net/stack.rs`)
- **Network Demo/Echo Server**: 106 lines (`kernel/src/net/demo.rs`)
- **Total**: 870 lines of networking code

## References

- [smoltcp Documentation](https://docs.rs/smoltcp)
- [Intel 82540EM Manual](https://www.intel.com/content/dam/doc/manual/pci-pci-x-family-gbe-controllers-software-dev-manual.pdf)
- [QEMU Networking](https://wiki.qemu.org/Documentation/Networking)
