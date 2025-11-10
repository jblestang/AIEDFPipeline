//! AIEDF scheduling library exposing DRR and EDF primitives plus the configuration surface for the
//! full pipeline.

pub mod drr_scheduler;
pub mod edf_scheduler;
pub mod egress_drr;
pub mod gui;
pub mod ingress_drr;
pub mod metrics;
pub mod multi_worker_edf;
pub mod pipeline;
pub mod queue;

// Re-export for easier testing
pub use pipeline::{
    CoreAssignment, EdfSchedulerConfig, IngressSchedulerConfig, Pipeline, PipelineConfig,
    QueueConfig, SchedulerKind, SocketConfig,
};
