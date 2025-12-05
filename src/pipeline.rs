//! Pipeline orchestration.
//!
//! This module wires the ingress DRR, EDF, and egress DRR schedulers together, exposes
//! configuration objects that make queue sizes and quantums tunable, and manages thread affinity
//! for the three-core deployment model.

use crossbeam_channel::{unbounded, Receiver};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

use crate::metrics::{MetricsCollector, MetricsSnapshot};
use crate::packet::Packet;
use crate::priority::{Priority, PriorityTable};
use crate::scheduler::cedf::CEDFScheduler;
use crate::scheduler::edf::EDFScheduler;
use crate::scheduler::egress_drr::EgressDRRScheduler;
use crate::scheduler::gedf::GEDFScheduler;
use crate::scheduler::gedf_vd::GEDFVDScheduler;
use crate::scheduler::ingress_drr::IngressDRRScheduler;
use crate::scheduler::mc_edf_elastic::MCEDFElasticScheduler;
use crate::scheduler::multi_worker_edf::MultiWorkerScheduler;
use crate::scheduler::single_cpu_edf::SingleCPUEDFScheduler;
use crate::threading::{set_cpu_affinity, set_thread_core, set_thread_priority};
use std::sync::atomic::{AtomicBool, Ordering};

/// Scheduler strategy selection for the EDF processing stage.
///
/// The pipeline supports four different EDF scheduling strategies, each optimized
/// for different workloads and performance characteristics:
///
/// - **Single**: Single-threaded EDF scheduler (legacy, simplest implementation)
///   - One thread processes all priorities sequentially
///   - Lowest overhead, suitable for low-throughput workloads
///   - No parallelism, may become a bottleneck under high load
///
/// - **MultiWorker**: Multi-worker EDF with adaptive load balancing (recommended)
///   - Multiple worker threads (3 by default) process packets in parallel
///   - Adaptive controller dynamically adjusts quotas based on observed processing times
///   - Per-priority FIFO ordering maintained via sequence tracking
///   - Best for high-throughput workloads requiring low latency
///
/// - **Global**: Global EDF with shared run queue
///   - Multiple worker threads share a single priority queue
///   - Workers compete for earliest-deadline packets
///   - Simpler than MultiWorker but may have more contention
///
/// - **GlobalVD**: Global EDF with virtual deadlines
///   - Similar to Global but uses virtual deadlines to prioritize high-priority tasks
///   - Virtual deadlines are computed as `original_deadline × scaling_factor`
///   - High priority gets smallest scaling factor (0.05), ensuring earliest virtual deadlines
///   - Best for workloads requiring strong high-priority task prioritization
///
/// - **Clairvoyant**: Clairvoyant EDF scheduler (multi-core optimized)
///   - Uses knowledge of task processing times and deadlines to make optimal scheduling decisions
///   - Dispatcher calculates slack time (deadline - current_time - estimated_processing) for each task
///   - Assigns tasks to workers using optimal heuristic (minimizes deadline misses, balances load)
///   - Each worker has its own queue ordered by slack time (least slack first)
///   - Best for workloads where processing times can be estimated accurately
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerKind {
    /// Single-threaded EDF scheduler (legacy, simplest)
    Single,
    /// Multi-worker EDF with adaptive load balancing (recommended for high throughput)
    MultiWorker,
    /// Global EDF with shared run queue
    Global,
    /// Global EDF with virtual deadlines (prioritizes high-priority tasks)
    GlobalVD,
    /// Clairvoyant EDF scheduler (multi-core optimized, uses slack time for optimal assignment)
    Clairvoyant,
    /// Multi-Channel EDF with elasticity (one EDF per priority, intelligent Round Robin for HIGH)
    MCEDFElastic,
    /// Single-CPU EDF scheduler with direct socket reading (reads directly from sockets, no ingress DRR)
    SingleCPUEDF,
}

impl Default for SchedulerKind {
    fn default() -> Self {
        SchedulerKind::Single
    }
}

/// UDP socket configuration shared by ingress and egress stages.
#[derive(Debug, Clone)]
pub struct SocketConfig {
    /// IP address (typically loopback).
    pub address: String,
    /// UDP port to bind/connect.
    pub port: u16,
    /// Latency budget consumed by EDF for deadline calculations.
    pub latency_budget: Duration,
    /// Priority associated with the socket (maps to scheduler queues).
    pub priority: Priority,
}

/// Core assignment for the three main runtime threads.
#[derive(Debug, Clone)]
pub struct CoreAssignment {
    /// Core used by the ingress DRR runtime.
    pub ingress: usize,
    /// Core dedicated to the EDF dispatcher when using the multi-worker scheduler.
    pub edf_dispatcher: usize,
    /// Worker cores used by the EDF multi-worker scheduler (falls back to dispatcher core when empty).
    pub edf_workers: Vec<usize>,
    /// Core used by egress plus metrics/statistics (best effort lane).
    pub egress: usize,
}

impl Default for CoreAssignment {
    fn default() -> Self {
        Self {
            ingress: 0,
            edf_dispatcher: 1,
            edf_workers: vec![1, 2],
            egress: 2,
        }
    }
}

/// Channel capacity configuration linking the schedulers.
#[derive(Debug, Clone)]
pub struct QueueConfig {
    /// Capacity of ingress → EDF channels, per priority.
    pub ingress_to_edf: PriorityTable<usize>,
    /// Capacity of EDF → egress channels, per priority.
    pub edf_to_egress: PriorityTable<usize>,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            ingress_to_edf: PriorityTable::from_fn(|priority| match priority {
                Priority::High => 16,
                Priority::Medium | Priority::Low | Priority::BestEffort => 128,
            }),
            edf_to_egress: PriorityTable::from_fn(|priority| match priority {
                Priority::High => 16,
                Priority::Medium | Priority::Low | Priority::BestEffort => 128,
            }),
        }
    }
}

/// Ingress DRR parameters.
#[derive(Debug, Clone)]
pub struct IngressSchedulerConfig {
    /// Packet quantum (in units of packets) granted to each priority per round.
    pub quantums: PriorityTable<usize>,
}

impl Default for IngressSchedulerConfig {
    fn default() -> Self {
        Self {
            quantums: PriorityTable::from_fn(|priority| match priority {
                Priority::High => 64,
                Priority::Medium => 8,
                Priority::Low | Priority::BestEffort => 1,
            }),
        }
    }
}

/// EDF scheduler configuration.
#[derive(Debug, Clone)]
pub struct EdfSchedulerConfig {
    /// Maximum heap size retained for compatibility; pending buffer remains one per priority.
    pub max_heap_size: usize,
}

impl Default for EdfSchedulerConfig {
    fn default() -> Self {
        Self { max_heap_size: 1 }
    }
}

/// Top-level pipeline configuration used during startup.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// CPU core layout (ingress, EDF, egress/metrics).
    pub cores: CoreAssignment,
    /// Channel capacities connecting schedulers.
    pub queues: QueueConfig,
    /// Ingress DRR tuning knobs.
    pub ingress: IngressSchedulerConfig,
    /// EDF tuning knobs.
    pub edf: EdfSchedulerConfig,
    /// Scheduler strategy (single-thread EDF vs multi-worker EDF).
    pub scheduler: SchedulerKind,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            cores: CoreAssignment::default(),
            queues: QueueConfig::default(),
            ingress: IngressSchedulerConfig::default(),
            edf: EdfSchedulerConfig::default(),
            scheduler: SchedulerKind::default(),
        }
    }
}

/// Complete pipeline wiring that owns schedulers, sockets, and metrics collectors.
///
/// The `Pipeline` struct orchestrates the entire scheduling stack:
/// - **Ingress DRR**: Reads packets from UDP sockets and routes them to EDF schedulers
/// - **EDF Schedulers**: Process packets using one of four scheduling strategies (Single, MultiWorker, Global, GlobalVD)
/// - **Egress DRR**: Emits processed packets to UDP sockets and records metrics
/// - **Metrics Collector**: Aggregates latency statistics and broadcasts them via TCP
///
/// The pipeline supports multiple scheduler types selected via `SchedulerKind`:
/// - `Single`: Single-threaded EDF (legacy, simplest)
/// - `MultiWorker`: Multi-worker EDF with adaptive load balancing (recommended for high throughput)
/// - `Global`: Global EDF with shared run queue
/// - `GlobalVD`: Global EDF with virtual deadlines (prioritizes high-priority tasks)
///
/// Threads are spawned via `run()` and can be shut down gracefully via `shutdown()`.
pub struct Pipeline {
    ingress_drr: Arc<IngressDRRScheduler>,
    edf_single: Option<Arc<EDFScheduler>>,
    edf_multi: Option<Arc<MultiWorkerScheduler>>,
    edf_global: Option<Arc<GEDFScheduler>>,
    edf_global_vd: Option<Arc<GEDFVDScheduler>>,
    edf_cedf: Option<Arc<CEDFScheduler>>,
    edf_mcedf: Option<Arc<MCEDFElasticScheduler>>,
    edf_single_cpu: Option<Arc<SingleCPUEDFScheduler>>,
    egress_drr: Arc<EgressDRRScheduler>,
    metrics_collector: Arc<MetricsCollector>,
    metrics_receiver: Receiver<std::collections::HashMap<u64, MetricsSnapshot>>,
    running: Arc<AtomicBool>, // Use AtomicBool instead of Mutex for lock-free reads
    input_sockets: Vec<SocketConfig>,
    output_sockets: Vec<SocketConfig>,
    ingress_drr_for_drops: Arc<IngressDRRScheduler>,
    scheduler_kind: SchedulerKind,
    config: PipelineConfig,
}

impl Pipeline {
    /// Build the pipeline using the supplied configuration without starting any threads yet.
    pub async fn new(config: PipelineConfig) -> Result<Self, Box<dyn std::error::Error>> {
        set_cpu_affinity()?;

        // Create multiple sockets per priority for load balancing and redundancy
        // High priority: ports 8080-8081 (2 sockets)
        // Medium priority: ports 8082-8083 (2 sockets)
        // Low priority: ports 8084-8085 (2 sockets)
        let input_sockets = vec![
            // High priority sockets
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 8080,
                latency_budget: Duration::from_millis(1),
                priority: Priority::High,
            },
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 8081,
                latency_budget: Duration::from_millis(1),
                priority: Priority::High,
            },
            // Medium priority sockets
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 8082,
                latency_budget: Duration::from_millis(10),
                priority: Priority::Medium,
            },
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 8083,
                latency_budget: Duration::from_millis(10),
                priority: Priority::Medium,
            },
            // Low priority sockets
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 8084,
                latency_budget: Duration::from_millis(100),
                priority: Priority::Low,
            },
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 8085,
                latency_budget: Duration::from_millis(100),
                priority: Priority::Low,
            },
        ];

        // Create multiple output sockets per priority for load balancing
        // High priority: ports 9080-9081 (2 sockets)
        // Medium priority: ports 9082-9083 (2 sockets)
        // Low priority: ports 9084-9085 (2 sockets)
        let output_sockets = vec![
            // High priority sockets
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 9080,
                latency_budget: Duration::from_millis(1),
                priority: Priority::High,
            },
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 9081,
                latency_budget: Duration::from_millis(1),
                priority: Priority::High,
            },
            // Medium priority sockets
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 9082,
                latency_budget: Duration::from_millis(10),
                priority: Priority::Medium,
            },
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 9083,
                latency_budget: Duration::from_millis(10),
                priority: Priority::Medium,
            },
            // Low priority sockets
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 9084,
                latency_budget: Duration::from_millis(100),
                priority: Priority::Low,
            },
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 9085,
                latency_budget: Duration::from_millis(100),
                priority: Priority::Low,
            },
        ];

        let mut latency_table = PriorityTable::from_fn(|_| Duration::from_millis(200));
        for socket in &input_sockets {
            latency_table[socket.priority] = socket.latency_budget;
        }
        for socket in &output_sockets {
            latency_table[socket.priority] = socket.latency_budget;
        }

        let (ingress_input_txs, ingress_input_rxs) =
            build_priority_channels(&config.queues.ingress_to_edf);
        let (edf_output_txs, edf_output_rxs) =
            build_priority_channels(&config.queues.edf_to_egress);

        let ingress_drr = Arc::new(IngressDRRScheduler::new(ingress_input_txs.clone()));
        let scheduler_kind = config.scheduler;
        let (edf_single, edf_multi, edf_global, edf_global_vd, edf_cedf, edf_mcedf, edf_single_cpu): (
            Option<Arc<EDFScheduler>>,
            Option<Arc<MultiWorkerScheduler>>,
            Option<Arc<GEDFScheduler>>,
            Option<Arc<GEDFVDScheduler>>,
            Option<Arc<CEDFScheduler>>,
            Option<Arc<MCEDFElasticScheduler>>,
            Option<Arc<SingleCPUEDFScheduler>>,
        ) = match scheduler_kind {
            SchedulerKind::Single => {
                let edf = Arc::new(EDFScheduler::new(
                    PriorityTable::from_fn(|priority| {
                        Arc::new(ingress_input_rxs[priority].clone())
                    }),
                    edf_output_txs.clone(),
                    config.edf.max_heap_size,
                ));
                (Some(edf), None, None, None, None, None, None)
            }
            SchedulerKind::MultiWorker => {
                let multi = Arc::new(MultiWorkerScheduler::new(
                    PriorityTable::from_fn(|priority| {
                        Arc::new(ingress_input_rxs[priority].clone())
                    }),
                    edf_output_txs.clone(),
                    latency_table.clone(),
                ));
                (None, Some(multi), None, None, None, None, None)
            }
            SchedulerKind::Global => {
                let global = Arc::new(GEDFScheduler::new(
                    PriorityTable::from_fn(|priority| {
                        Arc::new(ingress_input_rxs[priority].clone())
                    }),
                    edf_output_txs.clone(),
                ));
                (None, None, Some(global), None, None, None, None)
            }
            SchedulerKind::GlobalVD => {
                let global_vd = Arc::new(GEDFVDScheduler::new(
                    PriorityTable::from_fn(|priority| {
                        Arc::new(ingress_input_rxs[priority].clone())
                    }),
                    edf_output_txs.clone(),
                ));
                (None, None, None, Some(global_vd), None, None, None)
            }
            SchedulerKind::Clairvoyant => {
                let cedf = Arc::new(CEDFScheduler::new(
                    PriorityTable::from_fn(|priority| {
                        Arc::new(ingress_input_rxs[priority].clone())
                    }),
                    edf_output_txs.clone(),
                ));
                (None, None, None, None, Some(cedf), None, None)
            }
            SchedulerKind::MCEDFElastic => {
                let mcedf = Arc::new(MCEDFElasticScheduler::new(
                    PriorityTable::from_fn(|priority| {
                        Arc::new(ingress_input_rxs[priority].clone())
                    }),
                    edf_output_txs.clone(),
                ));
                (None, None, None, None, None, Some(mcedf), None)
            }
            SchedulerKind::SingleCPUEDF => {
                // Note: SingleCPUEDF reads directly from sockets, so it needs special socket configuration
                // For now, create a placeholder - full integration would require socket configuration setup
                // TODO: Integrate SingleCPUEDFScheduler with proper socket configuration
                let single_cpu_edf = Arc::new(SingleCPUEDFScheduler::new(
                    edf_output_txs.clone(),
                ));
                (None, None, None, None, None, None, Some(single_cpu_edf))
            }
        };
        let egress_drr = Arc::new(EgressDRRScheduler::new(edf_output_rxs.clone()));

        let (metrics_tx, metrics_rx) = unbounded();
        let metrics_collector = Arc::new(MetricsCollector::new(metrics_tx));

        Ok(Self {
            ingress_drr: ingress_drr.clone(),
            edf_single: edf_single.clone(),
            edf_multi: edf_multi.clone(),
            edf_global: edf_global.clone(),
            edf_global_vd: edf_global_vd.clone(),
            edf_cedf: edf_cedf.clone(),
            edf_mcedf: edf_mcedf.clone(),
            edf_single_cpu: edf_single_cpu.clone(),
            egress_drr,
            metrics_collector,
            metrics_receiver: metrics_rx,
            running: Arc::new(AtomicBool::new(false)),
            input_sockets,
            output_sockets,
            ingress_drr_for_drops: ingress_drr,
            scheduler_kind,
            config,
        })
    }

    /// Start the TCP metrics server that feeds the GUI.
    ///
    /// Spawns two Tokio tasks: one that converts internal metrics snapshots into JSON strings and
    /// another that accepts TCP clients, forwarding the broadcast stream to each connection.
    pub async fn start_metrics_server(
        &self,
        bind_addr: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use tokio::sync::broadcast;

        let listener = tokio::net::TcpListener::bind(bind_addr).await?;

        let metrics_rx = self.metrics_receiver.clone();
        let running_metrics = self.running.clone();
        let running_accept = self.running.clone();

        // Create a broadcast channel for metrics
        let (tx, _) = broadcast::channel::<String>(100);
        let metrics_tx = tx.clone();
        let tx_for_accept = tx.clone();

        // Spawn task to receive metrics and broadcast them
        tokio::spawn(async move {
            loop {
                match metrics_rx.try_recv() {
                    Ok(metrics) => {
                        match serde_json::to_string(&metrics) {
                            Ok(json) => {
                                let json_with_newline = format!("{}\n", json);
                                let _ = metrics_tx.send(json_with_newline);
                            }
                            Err(_e) => {
                                // Error serializing, continue
                            }
                        }
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => {
                        tokio::task::yield_now().await;
                    }
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        break;
                    }
                }

                if !running_metrics.load(Ordering::Relaxed) {
                    break;
                }
            }
        });

        // Spawn task to accept connections and send metrics
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((stream, _addr)) => {
                                let mut rx_clone = tx_for_accept.subscribe();
                                tokio::spawn(async move {
                                    let mut client_stream = stream;
                                    while let Ok(json) = rx_clone.recv().await {
                                        if client_stream.write_all(json.as_bytes()).await.is_err() {
                                            break;
                                        }
                                    }
                                });
                            }
                            Err(_) => {
                                // Error accepting, continue
                            }
                        }
                    }
                    _ = tokio::task::yield_now() => {
                        if !running_accept.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                }
            }
        });

        Ok(())
    }

    /// Launch the ingress, EDF, and egress threads plus the statistics publisher.
    ///
    /// This method orchestrates the entire pipeline startup:
    /// 1. Binds UDP input sockets and registers them with IngressDRRScheduler
    /// 2. Binds UDP output sockets and registers them with EgressDRRScheduler
    /// 3. Spawns IngressDRR thread (reads from UDP, routes to priority queues)
    /// 4. Spawns EDF scheduler threads (processes packets based on `SchedulerKind`)
    /// 5. Spawns EgressDRR thread (reads from priority queues, writes to UDP, records metrics)
    /// 6. Spawns statistics thread (periodically sends metrics updates to GUI)
    /// 7. Waits until `shutdown()` is called (blocks until shutdown signal)
    ///
    /// # Thread Layout
    ///
    /// - **Ingress-DRR**: Pinned to `cores.ingress`, priority 2 (high)
    /// - **EDF-Processor/Workers**: Pinned to `cores.edf_dispatcher`/`cores.edf_workers`, priority 2 (high)
    /// - **Egress-DRR**: Pinned to `cores.egress`, priority 1 (medium)
    /// - **Statistics-Thread**: Pinned to `cores.egress`, priority 1 (medium, best-effort)
    ///
    /// # Returns
    /// `Ok(())` when shutdown is requested, or an error if thread spawning fails
    ///
    /// # Errors
    /// Returns an error if socket binding fails or thread spawning fails
    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.running.store(true, Ordering::Relaxed);

        // Create and bind input sockets
        // For SingleCPUEDF: add sockets directly to the scheduler (replaces ingress DRR)
        // For other schedulers: add sockets to IngressDRRScheduler
        if self.scheduler_kind == SchedulerKind::SingleCPUEDF {
            // SingleCPUEDF reads directly from sockets, bypassing ingress DRR
            if let Some(ref single_cpu_edf) = self.edf_single_cpu {
                for config in &self.input_sockets {
                    let addr = format!("{}:{}", config.address, config.port);
                    let std_socket = std::net::UdpSocket::bind(&addr)?;
                    std_socket.set_nonblocking(true)?;
                    let socket = Arc::new(std_socket);
                    
                    single_cpu_edf.add_socket(socket, config.priority, config.latency_budget);
                }
            }
        } else {
            // Other schedulers use ingress DRR
            for config in &self.input_sockets {
                let addr = format!("{}:{}", config.address, config.port);
                let std_socket = std::net::UdpSocket::bind(&addr)?;
                std_socket.set_nonblocking(true)?;
                let socket = Arc::new(std_socket);

                let priority = config.priority;
                let quantum = self.config.ingress.quantums[priority];

                self.ingress_drr
                    .add_socket(socket, priority, config.latency_budget, quantum);
            }
        }

        // Create output sockets and add to EgressDRRScheduler
        for config in &self.output_sockets {
            let addr = format!("{}:{}", config.address, config.port);
            let socket_addr = addr.parse::<SocketAddr>()?;
            let socket = Arc::new(std::net::UdpSocket::bind("0.0.0.0:0")?);

            self.egress_drr
                .add_output_socket(config.priority, socket, socket_addr);
        }

        // Start IngressDRRScheduler (reads from UDP sockets, routes to priority queues)
        // Skip ingress DRR for SingleCPUEDF since it reads directly from sockets
        if self.scheduler_kind != SchedulerKind::SingleCPUEDF {
            let ingress_drr_clone = self.ingress_drr.clone();
            let running_ingress = self.running.clone();
            let ingress_core = self.config.cores.ingress;
            std::thread::Builder::new()
                .name("Ingress-DRR".to_string())
                .spawn(move || {
                    set_thread_priority(2);
                    set_thread_core(ingress_core);
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    rt.block_on(async move {
                        let _ = ingress_drr_clone.process_sockets(running_ingress).await;
                    });
                })?;
        }

        match self.scheduler_kind {
            SchedulerKind::Single => {
                let edf_clone = self
                    .edf_single
                    .as_ref()
                    .expect("single EDF scheduler must be present")
                    .clone();
                let running_edf = self.running.clone();
                let edf_core = self.config.cores.edf_dispatcher;

                std::thread::Builder::new()
                    .name("EDF-Processor".to_string())
                    .spawn(move || {
                        set_thread_priority(2);
                        set_thread_core(edf_core);
                        while running_edf.load(Ordering::Relaxed) {
                            if edf_clone.process_next().is_none() {
                                std::hint::spin_loop();
                            }
                        }
                    })?;
            }
            SchedulerKind::MultiWorker => {
                let scheduler = self
                    .edf_multi
                    .as_ref()
                    .expect("multi-worker scheduler must be present")
                    .clone();
                let running_multi = self.running.clone();
                let worker_cores = if self.config.cores.edf_workers.is_empty() {
                    vec![self.config.cores.edf_dispatcher]
                } else {
                    self.config.cores.edf_workers.clone()
                };
                scheduler.spawn_threads(
                    running_multi,
                    set_thread_priority,
                    set_thread_core,
                    &worker_cores,
                );
            }
            SchedulerKind::Global => {
                let scheduler = self
                    .edf_global
                    .as_ref()
                    .expect("global EDF scheduler must be present")
                    .clone();
                let running_global = self.running.clone();
                let worker_cores = if self.config.cores.edf_workers.is_empty() {
                    vec![self.config.cores.edf_dispatcher]
                } else {
                    self.config.cores.edf_workers.clone()
                };
                scheduler.spawn_threads(
                    running_global,
                    set_thread_priority,
                    set_thread_core,
                    self.config.cores.edf_dispatcher,
                    &worker_cores,
                );
            }
            SchedulerKind::GlobalVD => {
                let scheduler = self
                    .edf_global_vd
                    .as_ref()
                    .expect("GEDF-VD scheduler must be present")
                    .clone();
                let running_global = self.running.clone();
                let worker_cores = if self.config.cores.edf_workers.is_empty() {
                    vec![self.config.cores.edf_dispatcher]
                } else {
                    self.config.cores.edf_workers.clone()
                };
                scheduler.spawn_threads(
                    running_global,
                    set_thread_priority,
                    set_thread_core,
                    self.config.cores.edf_dispatcher,
                    &worker_cores,
                );
            }
            SchedulerKind::Clairvoyant => {
                let scheduler = self
                    .edf_cedf
                    .as_ref()
                    .expect("CEDF scheduler must be present")
                    .clone();
                let running_cedf = self.running.clone();
                let worker_cores = if self.config.cores.edf_workers.is_empty() {
                    vec![self.config.cores.edf_dispatcher]
                } else {
                    self.config.cores.edf_workers.clone()
                };
                scheduler.spawn_threads(
                    running_cedf,
                    set_thread_priority,
                    set_thread_core,
                    self.config.cores.edf_dispatcher,
                    &worker_cores,
                );
            }
            SchedulerKind::MCEDFElastic => {
                let scheduler = self
                    .edf_mcedf
                    .as_ref()
                    .expect("MC-EDF scheduler must be present")
                    .clone();
                let running_mcedf = self.running.clone();
                let worker_cores = if self.config.cores.edf_workers.is_empty() {
                    vec![self.config.cores.edf_dispatcher]
                } else {
                    self.config.cores.edf_workers.clone()
                };
                scheduler.spawn_threads(
                    running_mcedf,
                    set_thread_priority,
                    set_thread_core,
                    self.config.cores.edf_dispatcher,
                    &worker_cores,
                );
            }
            SchedulerKind::SingleCPUEDF => {
                // SingleCPUEDF uses run() method instead of spawn_threads
                // It reads directly from sockets (replaces ingress DRR)
                let scheduler = self
                    .edf_single_cpu
                    .as_ref()
                    .expect("SingleCPU-EDF scheduler must be present")
                    .clone();
                let running_single_cpu = self.running.clone();
                let edf_core = self.config.cores.edf_dispatcher;
                
                std::thread::Builder::new()
                    .name("SingleCPU-EDF".to_string())
                    .spawn(move || {
                        set_thread_priority(2);
                        set_thread_core(edf_core);
                        // run() now takes &self (interior mutability)
                        scheduler.run(running_single_cpu);
                    })?;
            }
        }

        // Start EgressDRRScheduler (reads from priority queues, writes to UDP sockets)
        let egress_drr_clone = self.egress_drr.clone();
        let metrics_clone = self.metrics_collector.clone();
        let running_egress = self.running.clone();
        let egress_core = self.config.cores.egress;
        std::thread::Builder::new()
            .name("Egress-DRR".to_string())
            .spawn(move || {
                set_thread_priority(1);
                set_thread_core(egress_core);
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    let _ = egress_drr_clone
                        .process_queues(running_egress, metrics_clone)
                        .await;
                });
            })?;

        // Spawn a thread to periodically send metrics updates (even when no packets are processed)
        // This ensures the GUI always has current data
        // Use lower priority thread for statistics to not interfere with packet processing
        let metrics_collector_periodic = self.metrics_collector.clone();
        let running_periodic = self.running.clone();
        // Map priority to expected latency for metrics reporting
        let expected_latencies_arc = Arc::new(PriorityTable::from_fn(|priority| {
            let mut budget = Duration::from_millis(200);
            for socket in &self.input_sockets {
                if socket.priority == priority {
                    budget = socket.latency_budget;
                }
            }
            for socket in &self.output_sockets {
                if socket.priority == priority {
                    budget = budget.min(socket.latency_budget);
                }
            }
            budget
        }));
        let expected_clone = expected_latencies_arc.clone();
        let ingress_drr_for_drops = self.ingress_drr_for_drops.clone();
        let edf_single_for_drops = self.edf_single.clone();
        let edf_multi_for_drops = self.edf_multi.clone();
        let edf_global_for_drops = self.edf_global.clone();
        let edf_global_vd_for_drops = self.edf_global_vd.clone();
        let edf_cedf_for_drops = self.edf_cedf.clone();
        let edf_mcedf_for_drops = self.edf_mcedf.clone();
        let edf_single_cpu_for_drops = self.edf_single_cpu.clone();
        let scheduler_kind = self.scheduler_kind;
        let stats_core = self.config.cores.egress;
        std::thread::Builder::new()
            .name("Statistics-Thread".to_string())
            .spawn(move || {
                set_thread_priority(1); // Lower priority for statistics (EDF is 2)
                set_thread_core(stats_core);
                let mut interval = std::time::Instant::now();
                loop {
                    // Check shutdown flag first
                    if !running_periodic.load(Ordering::Relaxed) {
                        break;
                    }

                    // Use spin loop with periodic check instead of sleep
                    // This allows more responsive shutdown while still batching updates
                    if interval.elapsed() >= Duration::from_millis(100) {
                        // Collect drop counts from schedulers
                        let ingress_drops = ingress_drr_for_drops.get_drop_counts();
                        let edf_drops = match scheduler_kind {
                            SchedulerKind::Single => edf_single_for_drops
                                .as_ref()
                                .expect("single EDF should exist")
                                .get_drop_counts(),
                            SchedulerKind::MultiWorker => edf_multi_for_drops
                                .as_ref()
                                .expect("multi EDF should exist")
                                .get_drop_counts(),
                            SchedulerKind::Global => edf_global_for_drops
                                .as_ref()
                                .expect("global EDF should exist")
                                .get_drop_counts(),
                            SchedulerKind::GlobalVD => edf_global_vd_for_drops
                                .as_ref()
                                .expect("GEDF-VD should exist")
                                .get_drop_counts(),
                            SchedulerKind::Clairvoyant => edf_cedf_for_drops
                                .as_ref()
                                .expect("CEDF should exist")
                                .get_drop_counts(),
                            SchedulerKind::MCEDFElastic => edf_mcedf_for_drops
                                .as_ref()
                                .expect("MC-EDF should exist")
                                .get_drop_counts(),
                            SchedulerKind::SingleCPUEDF => edf_single_cpu_for_drops
                                .as_ref()
                                .expect("SingleCPU-EDF should exist")
                                .get_drop_counts(),
                        };
                        if let Some(multi) = edf_multi_for_drops.as_ref() {
                            metrics_collector_periodic.update_worker_stats(Some(multi.stats()));
                        } else {
                            metrics_collector_periodic.update_worker_stats(None);
                        }
                        metrics_collector_periodic.send_current_metrics(
                            expected_clone.as_ref(),
                            Some(ingress_drops.clone()),
                            Some((edf_drops.heap, edf_drops.output.clone())),
                        );
                        interval = std::time::Instant::now();
                    } else {
                        std::hint::spin_loop();
                    }
                }
            })?;

        // Keep the runtime alive - wait until shutdown is requested
        while self.running.load(Ordering::Relaxed) {
            tokio::task::yield_now().await;
        }

        Ok(())
    }

    /// Signal all pipeline threads to stop gracefully.
    ///
    /// Sets the shared `running` flag to `false`, which causes all threads to exit their
    /// main loops. This is a cooperative shutdown: threads check the flag periodically
    /// and exit when it becomes false.
    ///
    /// # Graceful Shutdown
    /// All threads (IngressDRR, EDF, EgressDRR, Statistics) check `running` in their
    /// main loops and will exit when this method is called. The shutdown is asynchronous:
    /// threads may take a short time to notice the flag change and exit.
    ///
    /// # Thread Safety
    /// Uses `AtomicBool` with `Relaxed` ordering for lock-free reads/writes. All threads
    /// can safely check the flag without blocking.
    pub async fn shutdown(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

/// Create bounded crossbeam channels for each priority class using the supplied capacities.
///
/// Builds a pair of `PriorityTable` structures containing `Sender` and `Receiver` channels
/// for each priority. The channels are bounded to the capacities specified in the config,
/// preventing unbounded memory growth while allowing backpressure.
///
/// # Arguments
/// * `capacities` - Per-priority channel capacities (from `QueueConfig`)
///
/// # Returns
/// A tuple `(PriorityTable<Sender<Packet>>, PriorityTable<Receiver<Packet>>)` containing
/// the senders and receivers for each priority class
///
/// # Usage
/// Used during pipeline construction to wire ingress → EDF and EDF → egress connections.
fn build_priority_channels(
    capacities: &PriorityTable<usize>,
) -> (
    PriorityTable<crossbeam_channel::Sender<Packet>>,
    PriorityTable<crossbeam_channel::Receiver<Packet>>,
) {
    let mut senders = Vec::new();
    let mut receivers = Vec::new();
    for priority in Priority::ALL {
        let capacity = capacities[priority];
        let (tx, rx) = crossbeam_channel::bounded(capacity);
        senders.push(tx);
        receivers.push(rx);
    }
    (
        PriorityTable::from_vec(senders),
        PriorityTable::from_vec(receivers),
    )
}
