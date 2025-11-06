use crossbeam_channel::{unbounded, Receiver};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::UdpSocket;

use crate::drr_scheduler::Priority;
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

// Make Pipeline accessible for testing
#[cfg(test)]
impl Pipeline {
    pub fn get_input_sockets(&self) -> &[SocketConfig] {
        &self.input_sockets
    }

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
}

impl Pipeline {
    pub async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        // Set CPU affinity to 3 cores
        set_cpu_affinity()?;

        // Define multiple input sockets with different latency budgets
        let input_sockets = vec![
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 8080,
                latency_budget: Duration::from_millis(5),
                priority: Priority::HIGH,
            },
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 8081,
                latency_budget: Duration::from_millis(50),
                priority: Priority::MEDIUM,
            },
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 8082,
                latency_budget: Duration::from_millis(100),
                priority: Priority::LOW,
            },
        ];

        // Define multiple output sockets with matching latency budgets
        let output_sockets = vec![
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 9080,
                latency_budget: Duration::from_millis(5),
                priority: Priority::HIGH,
            },
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 9081,
                latency_budget: Duration::from_millis(50),
                priority: Priority::MEDIUM,
            },
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 9082,
                latency_budget: Duration::from_millis(100),
                priority: Priority::LOW,
            },
        ];

        // Create priority queues for EDF input (from IngressDRRScheduler)
        let (high_priority_input_tx, high_priority_input_rx) = crossbeam_channel::bounded(128);
        let (medium_priority_input_tx, medium_priority_input_rx) = crossbeam_channel::bounded(128);
        let (low_priority_input_tx, low_priority_input_rx) = crossbeam_channel::bounded(128);

        // Create priority queues for EDF output (to EgressDRRScheduler)
        let (high_priority_output_tx, high_priority_output_rx) = crossbeam_channel::bounded(128);
        let (medium_priority_output_tx, medium_priority_output_rx) =
            crossbeam_channel::bounded(128);
        let (low_priority_output_tx, low_priority_output_rx) = crossbeam_channel::bounded(128);

        // Create IngressDRRScheduler (reads from UDP sockets, routes to priority queues)
        let ingress_drr = Arc::new(IngressDRRScheduler::new(
            high_priority_input_tx,
            medium_priority_input_tx,
            low_priority_input_tx,
        ));

        // Create EDF scheduler (input from 3 priority queues, output to 3 priority queues)
        let edf = Arc::new(EDFScheduler::new(
            Arc::new(high_priority_input_rx),
            Arc::new(medium_priority_input_rx),
            Arc::new(low_priority_input_rx),
            high_priority_output_tx,
            medium_priority_output_tx,
            low_priority_output_tx,
        ));

        // Create EgressDRRScheduler (reads from priority queues, writes to UDP sockets)
        let egress_drr = Arc::new(EgressDRRScheduler::new(
            high_priority_output_rx,
            medium_priority_output_rx,
            low_priority_output_rx,
        ));

        // Create metrics collector
        let (metrics_tx, metrics_rx) = unbounded();
        let metrics_collector = Arc::new(MetricsCollector::new(
            metrics_tx, None, // No queue references for now
            None,
        ));

        Ok(Self {
            ingress_drr,
            edf,
            egress_drr,
            metrics_collector,
            metrics_receiver: metrics_rx,
            running: Arc::new(AtomicBool::new(false)),
            input_sockets,
            output_sockets,
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

            // Determine quantum based on priority
            // Balanced quantums: HIGH gets more bandwidth but not overwhelming
            let quantum = match config.priority {
                Priority::HIGH => 32768,   // High quantum for HIGH priority (was 65536, too high)
                Priority::MEDIUM => 4096,   // Medium quantum for MEDIUM priority
                Priority::LOW => 1024,      // Lower quantum for LOW priority
            };

            // Derive flow_id from priority for backward compatibility with IngressDRRScheduler
            let flow_id = match config.priority {
                Priority::HIGH => 1,
                Priority::MEDIUM => 2,
                Priority::LOW => 3,
            };

            self.ingress_drr
                .add_socket(socket, flow_id, config.latency_budget, quantum);
        }

        // Create output sockets and add to EgressDRRScheduler
        for config in &self.output_sockets {
            let addr = format!("{}:{}", config.address, config.port);
            let socket_addr = addr.parse::<SocketAddr>()?;
            let socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
            
            // Derive flow_id from priority for backward compatibility with EgressDRRScheduler
            let flow_id = match config.priority {
                Priority::HIGH => 1,
                Priority::MEDIUM => 2,
                Priority::LOW => 3,
            };
            
            self.egress_drr
                .add_output_socket(flow_id, socket, socket_addr);
        }

        // Start IngressDRRScheduler (reads from UDP sockets, routes to priority queues)
        let ingress_drr_clone = self.ingress_drr.clone();
        let running_ingress = self.running.clone();
        tokio::spawn(async move {
            let _ = ingress_drr_clone.process_sockets(running_ingress).await;
        });

        // EDF processing thread with high priority
        let edf_clone = self.edf.clone();
        let running_edf = self.running.clone();

        std::thread::Builder::new()
            .name("EDF-Processor".to_string())
            .spawn(move || {
                set_thread_priority(2); // Higher priority for EDF
                while running_edf.load(Ordering::Relaxed) {
                    // EDF processes packets and routes to output priority queues
                    let _ = edf_clone.process_next();
                    // No packets available - use spin loop for minimal latency
                    std::hint::spin_loop();
                }
            })?;

        // Start EgressDRRScheduler (reads from priority queues, writes to UDP sockets)
        let egress_drr_clone = self.egress_drr.clone();
        let metrics_clone = self.metrics_collector.clone();
        let running_egress = self.running.clone();
        tokio::spawn(async move {
            let _ = egress_drr_clone
                .process_queues(running_egress, metrics_clone)
                .await;
        });

        // Spawn a thread to periodically send metrics updates (even when no packets are processed)
        // This ensures the GUI always has current data
        // Use lower priority thread for statistics to not interfere with packet processing
        let metrics_collector_periodic = self.metrics_collector.clone();
        let running_periodic = self.running.clone();
        // Map priority to expected latency, but convert to flow_id for metrics (backward compatibility)
        let flow_id_to_expected_latency: HashMap<u64, Duration> = self
            .output_sockets
            .iter()
            .map(|config| {
                let flow_id = match config.priority {
                    Priority::HIGH => 1,
                    Priority::MEDIUM => 2,
                    Priority::LOW => 3,
                };
                (flow_id, config.latency_budget)
            })
            .collect();
        let flow_id_to_expected_latency_arc = Arc::new(flow_id_to_expected_latency);
        let flow_map_clone = flow_id_to_expected_latency_arc.clone();
        std::thread::Builder::new()
            .name("Statistics-Thread".to_string())
            .spawn(move || {
                set_thread_priority(1); // Lower priority for statistics (EDF is 2)
                let mut interval = std::time::Instant::now();
                loop {
                    // Check shutdown flag first
                    if !running_periodic.load(Ordering::Relaxed) {
                        break;
                    }

                    // Use spin loop with periodic check instead of sleep
                    // This allows more responsive shutdown while still batching updates
                    if interval.elapsed() >= Duration::from_millis(100) {
                        let map: HashMap<u64, Duration> =
                            flow_map_clone.iter().map(|(k, v)| (*k, *v)).collect();
                        metrics_collector_periodic.send_current_metrics(&map);
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
