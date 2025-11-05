pub mod drr_scheduler;
pub mod edf_scheduler;
pub mod gui;
pub mod metrics;
pub mod pipeline;
pub mod queue;

// Re-export for easier testing
pub use pipeline::{Pipeline, SocketConfig};
