// GUI binary - connects to running pipeline via TCP metrics server

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== AIEDF Pipeline GUI ===");
    println!("Connecting to metrics server on 127.0.0.1:9999...");
    println!("Make sure the pipeline is running (cargo run --release)");
    println!();

    let shutdown_flag = Arc::new(AtomicBool::new(false));

    // Run GUI on main thread (required on macOS, works on Linux too)
    // The GUI will connect to the TCP metrics server
    aiedf_pipeline::gui::run_gui_client("127.0.0.1:9999", shutdown_flag);

    Ok(())
}
