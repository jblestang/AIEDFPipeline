// Example Rust client to send and receive packets
use std::io;
use std::net::UdpSocket;
use std::time::Duration;

fn main() -> io::Result<()> {
    // Input sockets (where we send data)
    let input_sockets = vec![
        ("127.0.0.1:8080", "Flow 1 (1ms latency)"),
        ("127.0.0.1:8081", "Flow 2 (50ms latency)"),
        ("127.0.0.1:8082", "Flow 3 (100ms latency)"),
    ];

    // Output sockets (where we receive data)
    let output_sockets = vec![
        ("127.0.0.1:9080", "Flow 1 (1ms latency)"),
        ("127.0.0.1:9081", "Flow 2 (50ms latency)"),
        ("127.0.0.1:9082", "Flow 3 (100ms latency)"),
    ];

    println!("=== AIEDF Pipeline Test Client ===\n");

    // Send test packets
    println!("Sending test packets...");
    for (addr, desc) in &input_sockets {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        let message = format!("Test data for {}", desc);
        socket.send_to(message.as_bytes(), addr)?;
        println!("  Sent to {}: {}", addr, desc);
        std::thread::sleep(Duration::from_millis(100));
    }

    println!("\nWaiting for responses...");
    println!("(Make sure the pipeline is running)\n");

    // Receive packets from output sockets
    for (addr, desc) in &output_sockets {
        let socket = UdpSocket::bind(addr)?;
        socket.set_read_timeout(Some(Duration::from_secs(2)))?;

        let mut buf = [0u8; 1024];
        match socket.recv_from(&mut buf) {
            Ok((size, src)) => {
                let data = String::from_utf8_lossy(&buf[..size]);
                println!("  Received from {} ({}): {}", addr, desc, data);
                println!("    Source: {}", src);
            }
            Err(e) => {
                println!("  Timeout or error on {}: {}", addr, e);
            }
        }
    }

    Ok(())
}
