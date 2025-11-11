// GUI binary - connects to running pipeline via TCP metrics server

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut metrics_addr = String::from("127.0.0.1:9999");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some((key, value)) = arg.split_once('=') {
            match key {
                "--server" | "--host" | "--metrics" => metrics_addr = value.to_string(),
                _ => {}
            }
        } else {
            match arg.as_str() {
                "--server" | "--host" | "--metrics" => {
                    if let Some(value) = args.next() {
                        metrics_addr = value;
                    }
                }
                "--help" | "-h" => {
                    println!("Usage: gui [--server <host:port>]");
                    println!("Default metrics server: {}", metrics_addr);
                    return Ok(());
                }
                other => metrics_addr = other.to_string(),
            }
        }
    }

    println!("=== AIEDF Pipeline GUI ===");
    println!("Connecting to metrics server on {}...", metrics_addr);
    println!("Make sure the pipeline is running with matching --metrics-bind/host");
    println!();

    let shutdown_flag = Arc::new(AtomicBool::new(false));

    // Run GUI on main thread (required on macOS, works on Linux too)
    // The GUI will connect to the TCP metrics server
    aiedf_pipeline::gui::run_gui_client(&metrics_addr, shutdown_flag);

    Ok(())
}
