// Main pipeline binary - runs the pipeline without GUI

mod drr_scheduler;
mod edf_scheduler;
mod metrics;
mod pipeline;
mod queue;

use pipeline::Pipeline;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Use a runtime handle for async operations
    let rt = tokio::runtime::Runtime::new()?;

    // Initialize pipeline
    let pipeline = Arc::new(rt.block_on(Pipeline::new())?);

    // Start metrics server on port 9999
    let pipeline_for_metrics = pipeline.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            if let Err(e) = pipeline_for_metrics.start_metrics_server(9999).await {
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
