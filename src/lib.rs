//! AIEDF scheduling library exposing DRR and EDF primitives plus the configuration surface for the
//! full pipeline.

pub mod buffer_pool;
#[cfg(feature = "gui")]
pub mod gui;
pub mod metrics;
pub mod packet;
pub mod pipeline;
pub mod priority;
pub mod scheduler;
pub mod threading;

// Re-export for easier testing
pub use pipeline::{
    CoreAssignment, EdfSchedulerConfig, IngressSchedulerConfig, Pipeline, PipelineConfig,
    QueueConfig, SchedulerKind, SocketConfig,
};
