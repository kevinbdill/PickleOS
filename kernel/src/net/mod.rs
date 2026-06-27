//! Networking subsystem for PickleOS
//!
//! This module provides TCP/IP networking capabilities using the smoltcp
//! embedded network stack. It includes:
//! - Network device abstraction
//! - Socket management
//! - Integration with device drivers (E1000)

pub mod demo;
pub mod stack;

use alloc::vec::Vec;

/// Network device trait - implemented by NIC drivers
pub trait NetworkDevice {
    /// Get the MAC address of this device
    fn mac_address(&self) -> [u8; 6];
    
    /// Transmit a packet
    fn transmit(&mut self, data: &[u8]) -> Result<(), &'static str>;
    
    /// Receive a packet if available
    fn receive(&mut self) -> Option<Vec<u8>>;
    
    /// Check if the link is up
    fn link_up(&self) -> bool;
}

/// Initialize the networking subsystem
pub fn init() {
    crate::serial_println!("net :: initializing networking subsystem");
    
    // Initialize E1000 NIC driver
    if let Err(e) = crate::driver::e1000::init() {
        crate::serial_println!("net :: WARNING: E1000 init failed: {}", e);
        crate::serial_println!("net :: networking will not be available");
        return;
    }
    
    // Initialize the network stack with E1000
    stack::init();
    
    crate::serial_println!("net :: networking initialized");
}
