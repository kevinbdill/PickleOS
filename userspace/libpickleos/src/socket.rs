//! High-level socket API for user-space networking.

use crate::syscall::{
    sys_socket, sys_bind, sys_connect, sys_listen, sys_send, sys_recv, sys_accept, sys_shutdown,
};

// Address family
pub const AF_INET: u64 = 2;

// Socket type
pub const SOCK_STREAM: u64 = 1;

// Protocol
pub const IPPROTO_TCP: u64 = 6;

/// IPv4 address
#[derive(Debug, Clone, Copy)]
pub struct Ipv4Addr {
    pub octets: [u8; 4],
}

impl Ipv4Addr {
    pub const fn new(a: u8, b: u8, c: u8, d: u8) -> Self {
        Ipv4Addr { octets: [a, b, c, d] }
    }
    
    pub const fn localhost() -> Self {
        Ipv4Addr::new(127, 0, 0, 1)
    }
    
    pub const fn any() -> Self {
        Ipv4Addr::new(0, 0, 0, 0)
    }
}

/// Socket address (IPv4 + port)
#[derive(Debug, Clone, Copy)]
pub struct SocketAddr {
    pub ip: Ipv4Addr,
    pub port: u16,
}

impl SocketAddr {
    pub const fn new(ip: Ipv4Addr, port: u16) -> Self {
        SocketAddr { ip, port }
    }
    
    /// Convert to sockaddr_in bytes (network byte order)
    fn to_bytes(&self) -> [u8; 16] {
        let mut bytes = [0u8; 16];
        // Family = AF_INET (2) in native byte order
        bytes[0] = 2;
        bytes[1] = 0;
        // Port in network byte order (big-endian)
        bytes[2] = (self.port >> 8) as u8;
        bytes[3] = (self.port & 0xFF) as u8;
        // IP address
        bytes[4..8].copy_from_slice(&self.ip.octets);
        bytes
    }
}

/// TCP Socket
pub struct TcpSocket {
    fd: u32,
}

impl TcpSocket {
    /// Create a new TCP socket
    pub fn new() -> Result<Self, &'static str> {
        let fd = sys_socket(AF_INET, SOCK_STREAM, 0)?;
        Ok(TcpSocket { fd })
    }
    
    /// Bind to a local address
    pub fn bind(&self, addr: SocketAddr) -> Result<(), &'static str> {
        let bytes = addr.to_bytes();
        sys_bind(self.fd, &bytes)
    }
    
    /// Connect to a remote address
    pub fn connect(&self, addr: SocketAddr) -> Result<(), &'static str> {
        let bytes = addr.to_bytes();
        sys_connect(self.fd, &bytes)
    }
    
    /// Listen for incoming connections
    pub fn listen(&self, backlog: u32) -> Result<(), &'static str> {
        sys_listen(self.fd, backlog)
    }

    /// Block until a peer connects, returning the connected socket.
    ///
    /// In the current simplified model the listening socket itself becomes the
    /// connection, so the returned `TcpSocket` shares this socket's fd.
    pub fn accept(&self) -> Result<TcpSocket, &'static str> {
        let fd = sys_accept(self.fd, None)?;
        Ok(TcpSocket { fd })
    }

    /// Block until a peer connects and also report its address.
    pub fn accept_from(&self) -> Result<(TcpSocket, SocketAddr), &'static str> {
        let mut addr = [0u8; 16];
        let fd = sys_accept(self.fd, Some(&mut addr))?;
        let port = ((addr[2] as u16) << 8) | (addr[3] as u16);
        let ip = Ipv4Addr::new(addr[4], addr[5], addr[6], addr[7]);
        Ok((TcpSocket { fd }, SocketAddr::new(ip, port)))
    }

    /// Shut down the connection (both directions).
    pub fn shutdown(&self) -> Result<(), &'static str> {
        sys_shutdown(self.fd, 0)
    }

    /// Receive into a buffer, retrying until at least one byte arrives or the
    /// peer closes. Yields the CPU between attempts so the kernel can poll the
    /// network stack.
    pub fn recv_blocking(&self, buffer: &mut [u8]) -> Result<usize, &'static str> {
        loop {
            match self.recv(buffer) {
                Ok(0) => crate::syscall::sys_yield(),
                other => return other,
            }
        }
    }
    
    /// Send data
    pub fn send(&self, data: &[u8]) -> Result<usize, &'static str> {
        sys_send(self.fd, data, 0)
    }
    
    /// Receive data
    pub fn recv(&self, buffer: &mut [u8]) -> Result<usize, &'static str> {
        sys_recv(self.fd, buffer, 0)
    }
    
    /// Send all data (blocking until complete)
    pub fn send_all(&self, mut data: &[u8]) -> Result<(), &'static str> {
        while !data.is_empty() {
            match self.send(data) {
                Ok(0) => return Err("connection closed"),
                Ok(n) => data = &data[n..],
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    
    /// Get the underlying file descriptor
    pub fn fd(&self) -> u32 {
        self.fd
    }
}

impl Drop for TcpSocket {
    fn drop(&mut self) {
        // Socket will be closed when FD is closed by task cleanup
    }
}
