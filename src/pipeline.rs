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
use crate::scheduler::edf::EDFScheduler;
use crate::scheduler::egress_drr::EgressDRRScheduler;
use crate::scheduler::gedf::GEDFScheduler;
use crate::scheduler::gedf_vd::GEDFVDScheduler;
use crate::scheduler::ingress_drr::IngressDRRScheduler;
use crate::scheduler::multi_worker_edf::MultiWorkerScheduler;
use crate::threading::{set_cpu_affinity, set_thread_core, set_thread_priority};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerKind {
    Single,
    MultiWorker,
    Global,
    GlobalVD,
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
                Priority::Medium | Priority::Low | Priority::BestEffort => 16,
            }),
            edf_to_egress: PriorityTable::from_fn(|priority| match priority {
                Priority::High => 16,
                Priority::Medium | Priority::Low | Priority::BestEffort => 64,
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
pub struct Pipeline {
    ingress_drr: Arc<IngressDRRScheduler>,
    edf_single: Option<Arc<EDFScheduler>>,
    edf_multi: Option<Arc<MultiWorkerScheduler>>,
    edf_global: Option<Arc<GEDFScheduler>>,
    edf_global_vd: Option<Arc<GEDFVDScheduler>>,
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

        let input_sockets = vec![
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 8080,
                latency_budget: Duration::from_millis(1),
                priority: Priority::High,
            },
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 8081,
                latency_budget: Duration::from_millis(10),
                priority: Priority::Medium,
            },
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 8082,
                latency_budget: Duration::from_millis(100),
                priority: Priority::Low,
            },
        ];

        let output_sockets = vec![
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 9080,
                latency_budget: Duration::from_millis(1),
                priority: Priority::High,
            },
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 9081,
                latency_budget: Duration::from_millis(10),
                priority: Priority::Medium,
            },
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 9082,
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
        let (edf_single, edf_multi, edf_global, edf_global_vd): (
            Option<Arc<EDFScheduler>>,
            Option<Arc<MultiWorkerScheduler>>,
            Option<Arc<GEDFScheduler>>,
            Option<Arc<GEDFVDScheduler>>,
        ) = match scheduler_kind {
            SchedulerKind::Single => {
                let edf = Arc::new(EDFScheduler::new(
                    PriorityTable::from_fn(|priority| {
                        Arc::new(ingress_input_rxs[priority].clone())
                    }),
                    edf_output_txs.clone(),
                    config.edf.max_heap_size,
                ));
                (Some(edf), None, None, None)
            }
            SchedulerKind::MultiWorker => {
                let multi = Arc::new(MultiWorkerScheduler::new(
                    PriorityTable::from_fn(|priority| {
                        Arc::new(ingress_input_rxs[priority].clone())
                    }),
                    edf_output_txs.clone(),
                    latency_table.clone(),
                ));
                (None, Some(multi), None, None)
            }
            SchedulerKind::Global => {
                let global = Arc::new(GEDFScheduler::new(
                    PriorityTable::from_fn(|priority| {
                        Arc::new(ingress_input_rxs[priority].clone())
                    }),
                    edf_output_txs.clone(),
                ));
                (None, None, Some(global), None)
            }
            SchedulerKind::GlobalVD => {
                let global_vd = Arc::new(GEDFVDScheduler::new(
                    PriorityTable::from_fn(|priority| {
                        Arc::new(ingress_input_rxs[priority].clone())
                    }),
                    edf_output_txs.clone(),
                ));
                (None, None, None, Some(global_vd))
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
    /// The method binds sockets, wires metrics, and then waits until `shutdown` flips the shared
    /// running flag.
    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.running.store(true, Ordering::Relaxed);

        // Create and bind input sockets, add to IngressDRRScheduler
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

        // Create output sockets and add to EgressDRRScheduler
        for config in &self.output_sockets {
            let addr = format!("{}:{}", config.address, config.port);
            let socket_addr = addr.parse::<SocketAddr>()?;
            let socket = Arc::new(std::net::UdpSocket::bind("0.0.0.0:0")?);

            self.egress_drr
                .add_output_socket(config.priority, socket, socket_addr);
        }

        // Start IngressDRRScheduler (reads from UDP sockets, routes to priority queues)
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

    /// Signal all pipeline threads to stop.
    pub async fn shutdown(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

/// Create bounded crossbeam channels for each priority class using the supplied capacities.
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
