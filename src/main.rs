// Main pipeline binary - runs the pipeline without GUI
//
// The binary creates a Tokio runtime, instantiates the pipeline with default configuration, starts
// the metrics TCP server, and keeps the pipeline alive until Ctrl+C is received.

mod buffer_pool;
mod metrics;
mod packet;
mod pipeline;
mod priority;
mod scheduler;
mod threading;

use pipeline::{Pipeline, PipelineConfig, SchedulerKind};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Command-line options parsed from program arguments.
struct CliOptions {
    /// Scheduler strategy to use (Single, MultiWorker, Global, or GlobalVD)
    scheduler: SchedulerKind,
    /// IP address and port for the metrics TCP server (default: "127.0.0.1:9999")
    metrics_bind: String,
}

/// Normalize a metrics bind address to include a port if missing.
///
/// If the address contains a colon (indicating a port is already specified), returns
/// it as-is. Otherwise, appends the default port `:9999`.
///
/// # Arguments
/// * `value` - IP address or IP:port string
///
/// # Returns
/// Normalized address string with port (e.g., "127.0.0.1" → "127.0.0.1:9999")
fn normalize_metrics_bind(value: &str) -> String {
    if value.contains(':') {
        value.to_string()
    } else {
        format!("{value}:9999")
    }
}

/// Parse command-line arguments into `CliOptions`.
///
/// Supports two argument formats:
/// - `--scheduler=<value>` or `--scheduler <value>`: Select scheduler type
/// - `--metrics-bind=<value>` or `--metrics-bind <value>`: Set metrics server bind address
///
/// # Supported Scheduler Values
/// - `single`, `legacy`: Single-threaded EDF scheduler
/// - `multi`, `multi-worker`, `multi_worker`: Multi-worker EDF with adaptive balancing
/// - `gedf`, `g-edf`, `global`: Global EDF scheduler
/// - `gedf-vd`, `g-edf-vd`, `global-vd`: Global EDF with virtual deadlines
///
/// # Returns
/// `CliOptions` with parsed values (defaults to Single scheduler and "127.0.0.1:9999")
fn parse_cli_options() -> CliOptions {
    let mut scheduler = SchedulerKind::default();
    let mut metrics_bind = String::from("127.0.0.1:9999");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some((key, value)) = arg.split_once('=') {
            match key {
                "--scheduler" => {
                    scheduler = match value.to_ascii_lowercase().as_str() {
                        "multi" | "multi-worker" | "multi_worker" => SchedulerKind::MultiWorker,
                        "gedf" | "g-edf" | "global" => SchedulerKind::Global,
                        "gedf-vd" | "g-edf-vd" | "global-vd" => SchedulerKind::GlobalVD,
                        "single" | "legacy" => SchedulerKind::Single,
                        _ => scheduler,
                    };
                }
                "--metrics-bind" | "--metrics-host" => {
                    metrics_bind = normalize_metrics_bind(value);
                }
                _ => {}
            }
        } else if arg == "--scheduler" {
            if let Some(value) = args.next() {
                scheduler = match value.to_ascii_lowercase().as_str() {
                    "multi" | "multi-worker" | "multi_worker" => SchedulerKind::MultiWorker,
                    "gedf" | "g-edf" | "global" => SchedulerKind::Global,
                    "gedf-vd" | "g-edf-vd" | "global-vd" => SchedulerKind::GlobalVD,
                    "single" | "legacy" => SchedulerKind::Single,
                    _ => scheduler,
                };
            }
        } else if arg == "--metrics-bind" || arg == "--metrics-host" {
            if let Some(value) = args.next() {
                metrics_bind = normalize_metrics_bind(&value);
            }
        }
    }
    CliOptions {
        scheduler,
        metrics_bind,
    }
}

/// Main entry point for the pipeline binary.
///
/// This function:
/// 1. Parses command-line arguments to determine scheduler type and metrics bind address
/// 2. Creates a Tokio runtime for async operations
/// 3. Initializes the pipeline with the selected scheduler
/// 4. Spawns a background thread for the metrics TCP server
/// 5. Spawns a background thread for the pipeline runtime
/// 6. Waits for Ctrl+C signal
/// 7. Shuts down the pipeline gracefully
///
/// # Command-Line Arguments
/// - `--scheduler=<type>`: Select scheduler (single, multi, gedf, gedf-vd)
/// - `--metrics-bind=<addr>`: Set metrics server bind address (default: 127.0.0.1:9999)
///
/// # Example Usage
/// ```bash
/// # Run with multi-worker scheduler
/// cargo run -- --scheduler=multi
///
/// # Run with custom metrics server address
/// cargo run -- --metrics-bind=0.0.0.0:9999
/// ```
///
/// # Returns
/// `Ok(())` on successful shutdown, or an error if initialization fails
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = parse_cli_options();

    // Use a runtime handle for async operations
    let rt = tokio::runtime::Runtime::new()?;

    // Initialize pipeline
    let mut config = PipelineConfig::default();
    config.scheduler = options.scheduler;
    let pipeline = Arc::new(rt.block_on(Pipeline::new(config))?);

    // Start metrics server on port 9999
    let pipeline_for_metrics = pipeline.clone();
    let metrics_bind = options.metrics_bind.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let bind = metrics_bind;
        rt.block_on(async {
            if let Err(e) = pipeline_for_metrics.start_metrics_server(&bind).await {
                eprintln!("Failed to start metrics server: {}", e);
            }
            // Keep the runtime alive
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        });
    });

    // Start the pipeline in background
    let pipeline_clone = pipeline.clone();
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_flag_clone = shutdown_flag.clone();

    // Handle Ctrl+C
    ctrlc::set_handler(move || {
        shutdown_flag_clone.store(true, Ordering::Relaxed);
    })?;

    // Create a multi-threaded runtime for the pipeline
    let _pipeline_handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            if let Err(e) = pipeline_clone.run().await {
                eprintln!("Pipeline error: {}", e);
            }
        });
    });

    // Give the pipeline thread time to start
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Wait for shutdown signal
    while !shutdown_flag.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // Shutdown pipeline
    rt.block_on(pipeline.shutdown());

    Ok(())
}
