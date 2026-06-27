//! Network demonstration task
//!
//! This task demonstrates PickleOS networking capabilities by:
//! 1. Responding to ICMP ping requests (handled automatically by smoltcp)
//! 2. Running a TCP echo server on port 7
//!
//! The task continuously polls the network stack and processes incoming packets.

use crate::serial_println;
use crate::net::stack;

/// Network demo task - runs the network poll loop and TCP echo server
pub extern "C" fn network_demo_task() -> ! {
    serial_println!("[net-demo] Network demo task started");
    
    // Create TCP echo server socket
    let echo_socket = match stack::create_tcp_socket() {
        Ok(id) => id,
        Err(e) => {
            serial_println!("[net-demo] Failed to create TCP socket: {}", e);
            loop { crate::task::yield_now(); }
        }
    };
    
    // Listen on port 7 (TCP echo protocol)
    if let Err(e) = stack::tcp_listen(echo_socket, 7) {
        serial_println!("[net-demo] Failed to listen on port 7: {}", e);
        loop { crate::task::yield_now(); }
    }
    
    serial_println!("[net-demo] TCP echo server listening on port 7");
    serial_println!("[net-demo] ICMP echo (ping) responder active");
    serial_println!("[net-demo] Try: ping 10.0.2.15");
    serial_println!("[net-demo]     or: telnet localhost 5555");
    
    let mut buffer = [0u8; 1024];
    let mut was_connected = false;
    let mut connection_count = 0u32;
    
    loop {
        // Poll the network stack
        stack::poll();
        
        // Check connection status
        let is_connected = stack::tcp_is_connected(echo_socket);
        
        // Log new connections
        if is_connected && !was_connected {
            connection_count += 1;
            serial_println!("[net-demo] Connection #{} established", connection_count);
            was_connected = true;
        } else if !is_connected && was_connected {
            serial_println!("[net-demo] Connection closed");
            was_connected = false;
            // Re-listen for next connection
            stack::tcp_close(echo_socket);
            if let Err(e) = stack::tcp_listen(echo_socket, 7) {
                serial_println!("[net-demo] Failed to re-listen: {}", e);
            }
        }
        
        // Handle active connection
        if is_connected {
            // Try to receive data
            match stack::tcp_recv(echo_socket, &mut buffer) {
                Ok(0) => {
                    // No data available
                }
                Ok(n) => {
                    // Echo the data back
                    serial_println!("[net-demo] Received {} bytes, echoing back...", n);
                    
                    let mut sent = 0;
                    while sent < n {
                        match stack::tcp_send(echo_socket, &buffer[sent..n]) {
                            Ok(0) => {
                                // Can't send right now, poll and retry
                                stack::poll();
                                crate::task::yield_now();
                            }
                            Ok(m) => {
                                sent += m;
                            }
                            Err(e) => {
                                serial_println!("[net-demo] Send error: {}", e);
                                break;
                            }
                        }
                    }
                    
                    if sent == n {
                        serial_println!("[net-demo] Echo complete ({} bytes sent)", sent);
                    } else {
                        serial_println!("[net-demo] Partial echo ({}/{} bytes sent)", sent, n);
                    }
                }
                Err(e) => {
                    serial_println!("[net-demo] Recv error: {}", e);
                }
            }
        }
        
        // Yield to other tasks
        crate::task::yield_now();
    }
}
