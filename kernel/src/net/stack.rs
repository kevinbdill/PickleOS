//! TCP/IP stack integration using smoltcp
//!
//! This module wraps smoltcp's network stack and provides socket management
//! for PickleOS user-space programs.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use alloc::vec;
use spin::Mutex;
use smoltcp::iface::{Config, Interface, SocketSet, SocketHandle as SmoltcpHandle};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr, Ipv4Address};
use crate::driver::e1000;

/// Global network stack state
static STACK: Mutex<Option<NetworkStack>> = Mutex::new(None);

/// Network stack managing the interface and sockets
pub struct NetworkStack {
    interface: Interface,
    sockets: SocketSet<'static>,
    socket_map: BTreeMap<u32, SmoltcpHandle>,
    /// Local port a socket was bound to via `tcp_bind` (used by `tcp_listen`).
    bound_ports: BTreeMap<u32, u16>,
    next_socket_id: u32,
}

/// Maximum number of sockets allowed (prevents unbounded growth).
const MAX_SOCKETS: usize = 64;
pub type SocketId = u32;

/// E1000 device wrapper for smoltcp
struct E1000Device;

impl Device for E1000Device {
    type RxToken<'a> = E1000RxToken;
    type TxToken<'a> = E1000TxToken;
    
    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Check if we have a received packet
        if let Some(packet) = e1000::receive() {
            Some((E1000RxToken(packet), E1000TxToken))
        } else {
            None
        }
    }
    
    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(E1000TxToken)
    }
    
    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1500;
        caps.medium = Medium::Ethernet;
        caps
    }
}

struct E1000RxToken(Vec<u8>);
impl RxToken for E1000RxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = self.0;
        f(&mut buf)
    }
}

struct E1000TxToken;
impl TxToken for E1000TxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let result = f(&mut buf);
        
        // Transmit the packet
        if let Err(e) = e1000::transmit(&buf) {
            crate::serial_println!("net :: TX error: {}", e);
        }
        
        result
    }
}

impl NetworkStack {
    /// Create a new network stack with E1000 device
    fn new() -> Self {
        // Get MAC address from E1000 driver
        let mac_bytes = e1000::mac_address().unwrap_or([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
        let mac = EthernetAddress::from_bytes(&mac_bytes);
        let config = Config::new(mac.into());
        
        // Create the interface with E1000 device
        let mut interface = Interface::new(config, &mut E1000Device, Instant::from_millis(0));
        
        // Configure IP address (default to 10.0.2.15 like QEMU user networking)
        interface.update_ip_addrs(|ip_addrs| {
            ip_addrs
                .push(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24))
                .ok();
        });
        
        // Set default gateway
        interface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Address::new(10, 0, 2, 2))
            .ok();
        
        crate::serial_println!("net :: stack configured with IP 10.0.2.15/24, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac_bytes[0], mac_bytes[1], mac_bytes[2], mac_bytes[3], mac_bytes[4], mac_bytes[5]);
        crate::serial_println!("net :: gateway: 10.0.2.2");
        
        Self {
            interface,
            sockets: SocketSet::new(Vec::new()),
            socket_map: BTreeMap::new(),
            bound_ports: BTreeMap::new(),
            next_socket_id: 1,
        }
    }
    
    /// Create a new TCP socket
    pub fn create_tcp_socket(&mut self) -> Result<u32, &'static str> {
        if self.socket_map.len() >= MAX_SOCKETS {
            return Err("max sockets limit reached");
        }
        let tcp_rx_buffer = tcp::SocketBuffer::new(vec![0; 8192]);
        let tcp_tx_buffer = tcp::SocketBuffer::new(vec![0; 8192]);
        let tcp_socket = tcp::Socket::new(tcp_rx_buffer, tcp_tx_buffer);
        
        let handle = self.sockets.add(tcp_socket);
        let socket_id = self.next_socket_id;
        self.next_socket_id += 1;
        
        self.socket_map.insert(socket_id, handle);
        Ok(socket_id)
    }
    
    /// Create a new UDP socket
    pub fn create_udp_socket(&mut self) -> Result<u32, &'static str> {
        if self.socket_map.len() >= MAX_SOCKETS {
            return Err("max sockets limit reached");
        }
        let udp_rx_buffer = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; 16],
            vec![0; 4096],
        );
        let udp_tx_buffer = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; 16],
            vec![0; 4096],
        );
        let udp_socket = udp::Socket::new(udp_rx_buffer, udp_tx_buffer);
        
        let handle = self.sockets.add(udp_socket);
        let socket_id = self.next_socket_id;
        self.next_socket_id += 1;
        
        self.socket_map.insert(socket_id, handle);
        Ok(socket_id)
    }
    
    /// Bind a TCP socket to a local port (for listen)
    pub fn tcp_bind(&mut self, socket_id: u32, port: u16) -> Result<(), &'static str> {
        // For TCP, bind just records the port - the actual binding happens in
        // listen(). We validate the socket exists and remember the port so a
        // later `tcp_listen` can pick it up automatically.
        if !self.socket_map.contains_key(&socket_id) {
            return Err("invalid socket ID");
        }
        self.bound_ports.insert(socket_id, port);
        Ok(())
    }

    /// Return the local port a socket was previously bound to (if any).
    pub fn bound_port(&self, socket_id: u32) -> Option<u16> {
        self.bound_ports.get(&socket_id).copied()
    }
    
    /// Listen on a TCP socket
    pub fn tcp_listen(&mut self, socket_id: u32, port: u16) -> Result<(), &'static str> {
        let handle = self.socket_map.get(&socket_id).ok_or("invalid socket ID")?;
        let socket = self.sockets.get_mut::<tcp::Socket>(*handle);
        
        socket.listen(port).map_err(|_| "failed to listen")?;
        Ok(())
    }
    
    /// Connect to a remote TCP endpoint
    pub fn tcp_connect(&mut self, socket_id: u32, addr: Ipv4Address, port: u16, local_port: u16) -> Result<(), &'static str> {
        let handle = self.socket_map.get(&socket_id).ok_or("invalid socket ID")?;
        let socket = self.sockets.get_mut::<tcp::Socket>(*handle);
        
        use smoltcp::wire::IpEndpoint;
        let remote_endpoint = IpEndpoint::new(IpAddress::Ipv4(addr), port);
        let local_endpoint = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 15)), local_port);
        
        socket.connect(self.interface.context(), remote_endpoint, local_endpoint)
            .map_err(|_| "connect failed")?;
        Ok(())
    }
    
    /// Check if a TCP socket can accept a connection
    pub fn tcp_can_accept(&mut self, socket_id: u32) -> bool {
        if let Some(handle) = self.socket_map.get(&socket_id) {
            let socket = self.sockets.get_mut::<tcp::Socket>(*handle);
            socket.is_active()
        } else {
            false
        }
    }
    
    /// Read data from a TCP socket
    pub fn tcp_recv(&mut self, socket_id: u32, buffer: &mut [u8]) -> Result<usize, &'static str> {
        let handle = self.socket_map.get(&socket_id).ok_or("invalid socket ID")?;
        let socket = self.sockets.get_mut::<tcp::Socket>(*handle);
        
        if !socket.may_recv() {
            return Ok(0);
        }
        
        socket.recv_slice(buffer).map_err(|_| "recv error")
    }
    
    /// Write data to a TCP socket
    pub fn tcp_send(&mut self, socket_id: u32, data: &[u8]) -> Result<usize, &'static str> {
        let handle = self.socket_map.get(&socket_id).ok_or("invalid socket ID")?;
        let socket = self.sockets.get_mut::<tcp::Socket>(*handle);
        
        if !socket.may_send() {
            return Ok(0);
        }
        
        socket.send_slice(data).map_err(|_| "send error")
    }
    
    /// Check if TCP socket is connected
    pub fn tcp_is_connected(&mut self, socket_id: u32) -> bool {
        if let Some(handle) = self.socket_map.get(&socket_id) {
            let socket = self.sockets.get_mut::<tcp::Socket>(*handle);
            socket.is_active()
        } else {
            false
        }
    }
    
    /// Check whether a listening socket has an established connection ready.
    ///
    /// In smoltcp the listening socket itself transitions into the connected
    /// state once a peer completes the handshake, so "accept" is simply waiting
    /// for the socket to become active/connected.
    pub fn tcp_accept_ready(&mut self, socket_id: u32) -> bool {
        if let Some(handle) = self.socket_map.get(&socket_id) {
            let socket = self.sockets.get_mut::<tcp::Socket>(*handle);
            // `is_active` is true once the connection is established (and stays
            // true while it is being torn down); `may_recv`/`may_send` confirm
            // the data path is usable.
            socket.is_active() && (socket.may_recv() || socket.may_send())
        } else {
            false
        }
    }

    /// Return the remote (peer) endpoint of a connected socket, as
    /// (ip_octets, port), if available.
    pub fn tcp_remote_endpoint(&mut self, socket_id: u32) -> Option<([u8; 4], u16)> {
        let handle = self.socket_map.get(&socket_id)?;
        let socket = self.sockets.get_mut::<tcp::Socket>(*handle);
        let ep = socket.remote_endpoint()?;
        if let IpAddress::Ipv4(v4) = ep.addr {
            Some((v4.0, ep.port))
        } else {
            None
        }
    }

    /// Close a TCP socket
    pub fn tcp_close(&mut self, socket_id: u32) {
        if let Some(handle) = self.socket_map.get(&socket_id) {
            let socket = self.sockets.get_mut::<tcp::Socket>(*handle);
            socket.close();
        }
    }

    /// Fully destroy a TCP socket: close it, remove it from the socket set, and
    /// drop any bookkeeping. Used when a user-space file descriptor is closed.
    pub fn tcp_destroy(&mut self, socket_id: u32) {
        if let Some(handle) = self.socket_map.remove(&socket_id) {
            // Gracefully close then release the underlying smoltcp socket so its
            // buffers are reclaimed.
            {
                let socket = self.sockets.get_mut::<tcp::Socket>(handle);
                socket.close();
            }
            self.sockets.remove(handle);
        }
        self.bound_ports.remove(&socket_id);
    }
    
    /// Poll the network stack
    ///
    /// LOCK ORDERING: STACK must be acquired before DEVICE (the E1000
    /// device mutex in e1000.rs). Acquiring DEVICE while holding STACK
    /// is safe; the reverse (DEVICE then STACK) would deadlock.
    pub fn poll(&mut self) {
        let timestamp = Instant::from_millis(crate::task::scheduler::ticks() as i64);
        self.interface
            .poll(timestamp, &mut E1000Device, &mut self.sockets);
    }
}

/// Initialize the network stack
pub fn init() {
    let stack = NetworkStack::new();
    *STACK.lock() = Some(stack);
    crate::serial_println!("net :: smoltcp stack initialized");
}

/// Create a TCP socket
pub fn create_tcp_socket() -> Result<u32, &'static str> {
    let mut stack = STACK.lock();
    if let Some(ref mut s) = *stack {
        s.create_tcp_socket()
    } else {
        Err("network stack not initialized")
    }
}

/// Create a UDP socket
pub fn create_udp_socket() -> Result<u32, &'static str> {
    let mut stack = STACK.lock();
    if let Some(ref mut s) = *stack {
        s.create_udp_socket()
    } else {
        Err("network stack not initialized")
    }
}

/// Bind a TCP socket to a local port
pub fn tcp_bind(socket_id: u32, port: u16) -> Result<(), &'static str> {
    let mut stack = STACK.lock();
    if let Some(ref mut s) = *stack {
        s.tcp_bind(socket_id, port)
    } else {
        Err("network stack not initialized")
    }
}

/// Connect to a remote TCP endpoint
pub fn tcp_connect(socket_id: u32, addr: Ipv4Address, port: u16, local_port: u16) -> Result<(), &'static str> {
    let mut stack = STACK.lock();
    if let Some(ref mut s) = *stack {
        s.tcp_connect(socket_id, addr, port, local_port)
    } else {
        Err("network stack not initialized")
    }
}

/// Listen on a TCP socket
pub fn tcp_listen(socket_id: u32, port: u16) -> Result<(), &'static str> {
    let mut stack = STACK.lock();
    if let Some(ref mut s) = *stack {
        s.tcp_listen(socket_id, port)
    } else {
        Err("network stack not initialized")
    }
}

/// Check if a TCP socket can accept
pub fn tcp_can_accept(socket_id: u32) -> bool {
    let mut stack = STACK.lock();
    if let Some(ref mut s) = *stack {
        s.tcp_can_accept(socket_id)
    } else {
        false
    }
}

/// Receive data from TCP socket
pub fn tcp_recv(socket_id: u32, buffer: &mut [u8]) -> Result<usize, &'static str> {
    let mut stack = STACK.lock();
    if let Some(ref mut s) = *stack {
        s.tcp_recv(socket_id, buffer)
    } else {
        Err("network stack not initialized")
    }
}

/// Send data to TCP socket
pub fn tcp_send(socket_id: u32, data: &[u8]) -> Result<usize, &'static str> {
    let mut stack = STACK.lock();
    if let Some(ref mut s) = *stack {
        s.tcp_send(socket_id, data)
    } else {
        Err("network stack not initialized")
    }
}

/// Check if TCP socket is connected
pub fn tcp_is_connected(socket_id: u32) -> bool {
    let mut stack = STACK.lock();
    if let Some(ref mut s) = *stack {
        s.tcp_is_connected(socket_id)
    } else {
        false
    }
}

/// Close a TCP socket
pub fn tcp_close(socket_id: u32) {
    let mut stack = STACK.lock();
    if let Some(ref mut s) = *stack {
        s.tcp_close(socket_id);
    }
}

/// Fully destroy a TCP socket (close + free) — used on fd close.
pub fn tcp_destroy(socket_id: u32) {
    let mut stack = STACK.lock();
    if let Some(ref mut s) = *stack {
        s.tcp_destroy(socket_id);
    }
}

/// Return the local port a socket was bound to (via `tcp_bind`), if any.
pub fn bound_port(socket_id: u32) -> Option<u16> {
    let stack = STACK.lock();
    stack.as_ref().and_then(|s| s.bound_port(socket_id))
}

/// Check whether a listening socket has a connection ready to accept.
pub fn tcp_accept_ready(socket_id: u32) -> bool {
    let mut stack = STACK.lock();
    if let Some(ref mut s) = *stack {
        s.tcp_accept_ready(socket_id)
    } else {
        false
    }
}

/// Return the remote endpoint (ip octets, port) of a connected socket.
pub fn tcp_remote_endpoint(socket_id: u32) -> Option<([u8; 4], u16)> {
    let mut stack = STACK.lock();
    stack.as_mut().and_then(|s| s.tcp_remote_endpoint(socket_id))
}

/// Poll the network stack (should be called periodically)
pub fn poll() {
    let mut stack = STACK.lock();
    if let Some(ref mut s) = *stack {
        s.poll();
    }
}
