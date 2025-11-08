use crossbeam_channel::{unbounded, Receiver};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::UdpSocket;

use crate::drr_scheduler::{Packet, Priority, PriorityTable};
use crate::edf_scheduler::EDFScheduler;
use crate::egress_drr::EgressDRRScheduler;
use crate::ingress_drr::IngressDRRScheduler;
use crate::metrics::{MetricsCollector, MetricsSnapshot};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone)]
pub struct SocketConfig {
    pub address: String,
    pub port: u16,
    pub latency_budget: Duration,
    pub priority: Priority,
}

#[derive(Debug, Clone)]
pub struct CoreAssignment {
    pub ingress: usize,
    pub edf: usize,
    pub egress: usize,
}

impl Default for CoreAssignment {
    fn default() -> Self {
        Self {
            ingress: 0,
            edf: 1,
            egress: 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct QueueConfig {
    pub ingress_to_edf: PriorityTable<usize>,
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
                Priority::Medium | Priority::Low | Priority::BestEffort => 16,
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct IngressSchedulerConfig {
    pub quantums: PriorityTable<usize>,
}

impl Default for IngressSchedulerConfig {
    fn default() -> Self {
        Self {
            quantums: PriorityTable::from_fn(|priority| match priority {
                Priority::High => 16,
                Priority::Medium => 4,
                Priority::Low | Priority::BestEffort => 1,
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EdfSchedulerConfig {
    pub max_heap_size: usize,
}

impl Default for EdfSchedulerConfig {
    fn default() -> Self {
        Self { max_heap_size: 16 }
    }
}

#[derive(Debug, Clone)]
pub struct PipelineConfig {
    pub cores: CoreAssignment,
    pub queues: QueueConfig,
    pub ingress: IngressSchedulerConfig,
    pub edf: EdfSchedulerConfig,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            cores: CoreAssignment::default(),
            queues: QueueConfig::default(),
            ingress: IngressSchedulerConfig::default(),
            edf: EdfSchedulerConfig::default(),
        }
    }
}

// Make Pipeline accessible for testing
#[cfg(test)]
impl Pipeline {
    #[allow(dead_code)]
    pub fn get_input_sockets(&self) -> &[SocketConfig] {
        &self.input_sockets
    }

    #[allow(dead_code)]
    pub fn get_output_sockets(&self) -> &[SocketConfig] {
        &self.output_sockets
    }
}

pub struct Pipeline {
    ingress_drr: Arc<IngressDRRScheduler>,
    edf: Arc<EDFScheduler>,
    egress_drr: Arc<EgressDRRScheduler>,
    metrics_collector: Arc<MetricsCollector>,
    metrics_receiver: Receiver<std::collections::HashMap<u64, MetricsSnapshot>>,
    running: Arc<AtomicBool>, // Use AtomicBool instead of Mutex for lock-free reads
    input_sockets: Vec<SocketConfig>,
    output_sockets: Vec<SocketConfig>,
    // Store scheduler references for drop count collection
    ingress_drr_for_drops: Arc<IngressDRRScheduler>,
    edf_for_drops: Arc<EDFScheduler>,
    config: PipelineConfig,
}

impl Pipeline {
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

        let (ingress_input_txs, ingress_input_rxs) =
            build_priority_channels(&config.queues.ingress_to_edf);
        let (edf_output_txs, edf_output_rxs) =
            build_priority_channels(&config.queues.edf_to_egress);

        let ingress_drr = Arc::new(IngressDRRScheduler::new(ingress_input_txs.clone()));
        let edf = Arc::new(EDFScheduler::new(
            PriorityTable::from_fn(|priority| Arc::new(ingress_input_rxs[priority].clone())),
            edf_output_txs.clone(),
            config.edf.max_heap_size,
        ));
        let egress_drr = Arc::new(EgressDRRScheduler::new(edf_output_rxs.clone()));

        let (metrics_tx, metrics_rx) = unbounded();
        let metrics_collector = Arc::new(MetricsCollector::new(metrics_tx, None, None));

        Ok(Self {
            ingress_drr: ingress_drr.clone(),
            edf: edf.clone(),
            egress_drr,
            metrics_collector,
            metrics_receiver: metrics_rx,
            running: Arc::new(AtomicBool::new(false)),
            input_sockets,
            output_sockets,
            ingress_drr_for_drops: ingress_drr,
            edf_for_drops: edf,
            config,
        })
    }

    /// Start a TCP server that broadcasts metrics to connected clients
    /// This allows the GUI to connect as a separate process
    pub async fn start_metrics_server(&self, port: u16) -> Result<(), Box<dyn std::error::Error>> {
        use tokio::sync::broadcast;

        let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port)).await?;

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
            let socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);

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

        // EDF processing thread with high priority
        let edf_clone = self.edf.clone();
        let running_edf = self.running.clone();
        let edf_core = self.config.cores.edf;

        std::thread::Builder::new()
            .name("EDF-Processor".to_string())
            .spawn(move || {
                set_thread_priority(2);
                set_thread_core(edf_core);
                while running_edf.load(Ordering::Relaxed) {
                    let _ = edf_clone.process_next();
                    std::hint::spin_loop();
                }
            })?;

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
        let mut expected_latencies = PriorityTable::from_fn(|_| Duration::from_millis(200));
        for socket in &self.output_sockets {
            expected_latencies[socket.priority] = socket.latency_budget;
        }
        let expected_latencies_arc = Arc::new(expected_latencies);
        let expected_clone = expected_latencies_arc.clone();
        let ingress_drr_for_drops = self.ingress_drr_for_drops.clone();
        let edf_for_drops = self.edf_for_drops.clone();
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
                        let edf_drops = edf_for_drops.get_drop_counts();
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

    pub async fn shutdown(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

fn set_cpu_affinity() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    {
        use libc::{cpu_set_t, getpid, sched_setaffinity, CPU_SET, CPU_ZERO};

        unsafe {
            let mut set: cpu_set_t = std::mem::zeroed();
            CPU_ZERO(&mut set);
            CPU_SET(0, &mut set);
            CPU_SET(1, &mut set);
            CPU_SET(2, &mut set);

            let pid = getpid();
            let _ = sched_setaffinity(pid, std::mem::size_of::<cpu_set_t>(), &set);
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // CPU affinity not available on this platform
    }
    Ok(())
}

fn set_thread_priority(priority: i32) {
    #[cfg(target_os = "linux")]
    {
        use libc::{getpid, sched_param, sched_setscheduler, SCHED_FIFO};
        use std::mem;

        unsafe {
            let mut param: sched_param = mem::zeroed();
            param.sched_priority = priority;
            let pid = getpid();
            let _ = sched_setscheduler(pid, SCHED_FIFO, &param);
        }
    }
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        // macOS uses Quality of Service (QoS) classes instead of numeric priorities
        // Map priority levels to QoS classes:
        // Priority 2 (high) -> QOS_CLASS_USER_INTERACTIVE or QOS_CLASS_USER_INITIATED
        // Priority 1 (low) -> QOS_CLASS_UTILITY or QOS_CLASS_BACKGROUND

        // Define QoS class constants (from pthread/qos.h)
        // QOS_CLASS_USER_INTERACTIVE = 0x21 (highest, for UI)
        const QOS_CLASS_USER_INITIATED: u32 = 0x19; // High priority for critical work
        const QOS_CLASS_UTILITY: u32 = 0x15; // Medium priority for utility work
        const QOS_CLASS_BACKGROUND: u32 = 0x09; // Low priority for background work

        // Select QoS class based on priority
        let qos_class = if priority >= 2 {
            QOS_CLASS_USER_INITIATED // High priority for EDF and critical threads
        } else if priority == 1 {
            QOS_CLASS_UTILITY // Lower priority for statistics thread
        } else {
            QOS_CLASS_BACKGROUND // Lowest priority
        };

        unsafe {
            // Set QoS class for current thread
            // pthread_set_qos_class_self_np signature: int pthread_set_qos_class_self_np(qos_class_t qos_class, int relative_priority);
            extern "C" {
                fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
            }

            let _ = pthread_set_qos_class_self_np(qos_class, 0);

            // Also set thread name if possible
            if let Ok(name) = CString::new(format!("Thread-Priority-{}", priority)) {
                let _ = libc::pthread_setname_np(name.as_ptr());
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        // Thread priority setting not implemented for this platform
        let _ = priority;
    }
}

fn set_thread_core(core_id: usize) {
    #[cfg(target_os = "linux")]
    unsafe {
        use libc::{cpu_set_t, pthread_self, pthread_setaffinity_np, CPU_SET, CPU_ZERO};
        let mut set: cpu_set_t = std::mem::zeroed();
        CPU_ZERO(&mut set);
        CPU_SET(core_id, &mut set);
        let _ = pthread_setaffinity_np(pthread_self(), std::mem::size_of::<cpu_set_t>(), &set);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = core_id;
    }
}

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
