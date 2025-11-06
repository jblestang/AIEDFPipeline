use crossbeam_channel::{unbounded, Receiver};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::net::UdpSocket;

use crate::drr_scheduler::{DRRScheduler, Packet};
use crate::edf_scheduler::EDFScheduler;
use crate::metrics::{MetricsCollector, MetricsSnapshot};
use crate::queue::Queue;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone)]
pub struct SocketConfig {
    pub address: String,
    pub port: u16,
    pub latency_budget: Duration,
    pub flow_id: u64,
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
    input_drr: Arc<DRRScheduler>,
    edf: Arc<EDFScheduler>,
    output_drr: Arc<DRRScheduler>,
    queue1: Arc<Queue>,
    queue2: Arc<Queue>,
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
                flow_id: 1,
            },
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 8081,
                latency_budget: Duration::from_millis(50),
                flow_id: 2,
            },
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 8082,
                latency_budget: Duration::from_millis(100),
                flow_id: 3,
            },
        ];

        // Define multiple output sockets with matching latency budgets
        let output_sockets = vec![
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 9080,
                latency_budget: Duration::from_millis(5),
                flow_id: 1,
            },
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 9081,
                latency_budget: Duration::from_millis(50),
                flow_id: 2,
            },
            SocketConfig {
                address: "127.0.0.1".to_string(),
                port: 9082,
                latency_budget: Duration::from_millis(100),
                flow_id: 3,
            },
        ];

        // Create queues
        // Flow: UDP Input → Input DRR → Queue1 → EDF → Queue2 → Output DRR → UDP Output
        // (Queue3 removed - Output DRR now sends directly to UDP)
        let queue1 = Arc::new(Queue::new());
        let queue2 = Arc::new(Queue::new());

        // Create input DRR scheduler
        // Input DRR: receives packets from UDP sockets, schedules via DRR, outputs to queue1
        let (input_drr_tx, input_drr_rx) = unbounded();
        let input_drr = Arc::new(DRRScheduler::new(input_drr_tx));
        input_drr.set_receiver(input_drr_rx);

        // Create output DRR scheduler
        // Output DRR: receives packets from queue2, schedules via DRR, sends directly to UDP
        let (output_drr_tx, output_drr_rx) = unbounded();
        let output_drr = Arc::new(DRRScheduler::new(output_drr_tx));
        output_drr.set_receiver(output_drr_rx);

        // Create EDF scheduler (input from queue1, output to queue2)
        let edf = Arc::new(EDFScheduler::new(queue1.receiver(), queue2.sender()));

        // Create metrics collector with queue references for occupancy tracking
        let (metrics_tx, metrics_rx) = unbounded();
        let metrics_collector = Arc::new(MetricsCollector::new(
            metrics_tx,
            Some(queue1.clone()),
            Some(queue2.clone()),
        ));

        Ok(Self {
            input_drr,
            edf,
            output_drr,
            queue1,
            queue2,
            metrics_collector,
            metrics_receiver: metrics_rx,
            running: Arc::new(AtomicBool::new(false)),
            input_sockets,
            output_sockets,
        })
    }

    #[allow(dead_code)]
    pub fn get_metrics_receiver(
        &self,
    ) -> Receiver<std::collections::HashMap<u64, MetricsSnapshot>> {
        self.metrics_receiver.clone()
    }

    #[allow(dead_code)]
    pub fn get_metrics_collector(&self) -> Arc<MetricsCollector> {
        self.metrics_collector.clone()
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

        // Create and bind input sockets
        let mut input_socket_handles = Vec::new();
        for config in &self.input_sockets {
            let addr = format!("{}:{}", config.address, config.port);
            let socket = Arc::new(UdpSocket::bind(&addr).await?);
            input_socket_handles.push((socket, config.clone()));
        }

        // Configure flows with different latencies and quantums based on socket configs
        // Rebalance DRR quantums: different quantum values to ensure fair packet distribution
        // Quantum ratios: Flow 1: 32768, Flow 2: 4096, Flow 3: 1024 (32:4:1 ratio)
        // This compensates for flow 1's lower data rate and balances flow 2 vs flow 3
        for config in &self.input_sockets {
            let quantum = match config.flow_id {
                1 => 32768, // Highest quantum for flow 1 (low data rate) to ensure fair treatment
                2 => 4096,  // Medium quantum for flow 2
                3 => 1024,  // Standard quantum for flow 3
                _ => 1024,  // Default quantum for other flows
            };
            self.input_drr
                .add_flow(config.flow_id, quantum, config.latency_budget);
        }

        for config in &self.output_sockets {
            let quantum = match config.flow_id {
                1 => 32768, // Highest quantum for flow 1 (low data rate) to ensure fair treatment
                2 => 4096,  // Medium quantum for flow 2
                3 => 1024,  // Standard quantum for flow 3
                _ => 1024,  // Default quantum for other flows
            };
            self.output_drr
                .add_flow(config.flow_id, quantum, config.latency_budget);
        }

        // Create a map from flow_id to pre-computed SocketAddr for routing (avoids string formatting in hot path)
        let output_socket_map: HashMap<u64, SocketAddr> = self
            .output_sockets
            .iter()
            .filter_map(|config| {
                format!("{}:{}", config.address, config.port)
                    .parse::<SocketAddr>()
                    .ok()
                    .map(|addr| (config.flow_id, addr))
            })
            .collect();

        // Spawn input handlers for each socket
        // Flow 1 bypasses Input DRR and goes directly to queue1 for lowest latency
        // Flow 2/3 go through Input DRR for fair scheduling
        let queue1_clone_for_flow1 = self.queue1.clone();
        for (socket, config) in input_socket_handles {
            let socket_clone = socket.clone();
            let input_drr_clone = self.input_drr.clone();
            let running_clone = self.running.clone();
            let latency_budget = config.latency_budget;
            let flow_id = config.flow_id;
            let queue1_for_flow1 = if flow_id == 1 {
                Some(queue1_clone_for_flow1.clone())
            } else {
                None
            };

            let _handle = tokio::spawn(async move {
                let mut buf = [0u8; 1024];

                while running_clone.load(Ordering::Relaxed) {
                    match socket_clone.recv_from(&mut buf).await {
                        Ok((size, _addr)) => {
                            if size == 0 {
                                continue;
                            }
                            
                            // Optimized allocation: Vec::from() allocates with exact capacity
                            // This avoids overallocation that to_vec() might do in some cases
                            // Note: Still requires allocation, but with optimal capacity
                            let data = Vec::from(&buf[..size]);

                            let packet = Packet {
                                flow_id,
                                data,
                                timestamp: Instant::now(), // Record timestamp immediately when packet is received
                                latency_budget,
                            };

                            // Flow 1 bypasses Input DRR and goes directly to queue1 for lowest latency
                            if flow_id == 1 {
                                if let Some(queue1) = queue1_for_flow1.as_ref() {
                                    // Direct path for Flow 1 - bypass Input DRR entirely
                                    match queue1.sender().try_send(packet) {
                                        Ok(()) => {
                                            // Successfully sent
                                        }
                                        Err(crossbeam_channel::TrySendError::Full(packet)) => {
                                            // Queue is full - try to drop a Flow 2/3 packet to make room
                                            if let Ok(old_packet) = queue1.try_recv() {
                                                if old_packet.flow_id != 1 {
                                                    // Drop the old packet and send Flow 1 packet
                                                    let _ = queue1.sender().try_send(packet);
                                                } else {
                                                    // Old packet was also Flow 1, put it back
                                                    let _ = queue1.sender().try_send(old_packet);
                                                }
                                            }
                                            // If couldn't make room, Flow 1 packet is lost (shouldn't happen often)
                                        }
                                        Err(_) => {
                                            // Channel disconnected, continue
                                        }
                                    }
                                }
                            } else {
                                // Flow 2/3 go through Input DRR for fair scheduling
                                let _ = input_drr_clone.schedule_packet(packet);
                            }
                        }
                        Err(_e) => {
                            // Socket error - yield briefly
                            tokio::task::yield_now().await;
                        }
                    }
                }
            });
        }

        // Start input DRR processing thread
        // This thread processes packets from input DRR's internal channel and forwards to queue1
        let input_drr_clone = self.input_drr.clone();
        let queue1_clone = self.queue1.clone();
        let running_input_drr = self.running.clone();

        std::thread::Builder::new()
            .name("Input-DRR".to_string())
            .spawn(move || {
                while running_input_drr.load(Ordering::Relaxed) {
                    // Process packets from input DRR (DRR scheduling handles socket read priority)
                    if let Some(packet) = input_drr_clone.process_next() {
                        // Use try_send to avoid blocking on full queue
                        // If queue is full, prioritize Flow 1 by dropping Flow 2/3 packets if needed
                        let flow_id = packet.flow_id;
                        match queue1_clone.sender().try_send(packet) {
                            Ok(()) => {
                                // Successfully sent
                            }
                            Err(crossbeam_channel::TrySendError::Full(packet)) => {
                                // Queue is full - if this is Flow 1 (low latency), drop a Flow 2/3 packet to make room
                                if flow_id == 1 {
                                    // Try to receive and drop a non-Flow1 packet to make room
                                    if let Ok(old_packet) = queue1_clone.try_recv() {
                                        if old_packet.flow_id != 1 {
                                            // Drop the old packet and send Flow 1 packet
                                            let _ = queue1_clone.sender().try_send(packet);
                                        } else {
                                            // Old packet was also Flow 1, put it back and skip this one
                                            let _ = queue1_clone.sender().try_send(old_packet);
                                        }
                                    } else {
                                        // Couldn't make room, Flow 1 packet is lost (shouldn't happen often)
                                    }
                                }
                                // For Flow 2/3, if queue is full, just drop the packet
                                // This ensures Flow 1 packets can always get through
                            }
                            Err(_) => {
                                // Channel disconnected, exit
                                break;
                            }
                        }
                    } else {
                        // No packets available - yield briefly to avoid busy-waiting
                        // Use std::hint::spin_loop for better CPU efficiency without sleep overhead
                        std::hint::spin_loop();
                    }
                }
            })?;

        // EDF processing thread with high priority
        let edf_clone = self.edf.clone();
        let queue2_clone = self.queue2.clone();
        let running_clone = self.running.clone();

        std::thread::Builder::new()
            .name("EDF-Processor".to_string())
            .spawn(move || {
                set_thread_priority(2); // Higher priority for EDF
                while running_clone.load(Ordering::Relaxed) {
                    if let Some(packet) = edf_clone.process_next() {
                        // Removed verbose logging to reduce latency
                        // Minimal processing delay - EDF should process immediately
                        // No sleep needed for zero-copy processing

                        // Send processed packet via EDF's output (queue2)
                        // Use try_send to avoid blocking - if queue is full, prioritize Flow 1
                        let flow_id = packet.flow_id;
                        let queue2_sender = queue2_clone.sender();
                        match queue2_sender.try_send(packet) {
                            Ok(()) => {
                                // Successfully sent
                            }
                            Err(crossbeam_channel::TrySendError::Full(packet)) => {
                                // Queue is full - if this is Flow 1 (low latency), drop a Flow 2/3 packet to make room
                                if flow_id == 1 {
                                    // Try to receive and drop a non-Flow1 packet to make room
                                    if let Ok(old_packet) = queue2_clone.try_recv() {
                                        if old_packet.flow_id != 1 {
                                            // Drop the old packet and send Flow 1 packet
                                            let _ = queue2_sender.try_send(packet);
                                        } else {
                                            // Old packet was also Flow 1, put it back and skip this one
                                            let _ = queue2_sender.try_send(old_packet);
                                        }
                                    } else {
                                        // Couldn't make room, Flow 1 packet is lost (shouldn't happen often)
                                    }
                                }
                                // For Flow 2/3, if queue is full, just drop the packet
                            }
                            Err(_) => {
                                // Channel disconnected, continue
                            }
                        }
                    } else {
                        // No packets available - use spin loop for minimal latency
                        std::hint::spin_loop();
                    }
                }
            })?;

        // Output DRR + UDP output combined - receives from queue2, schedules via DRR, sends directly to UDP
        let queue2_clone = self.queue2.clone();
        let output_drr_clone = self.output_drr.clone();
        let metrics_clone = self.metrics_collector.clone();
        let running_clone = self.running.clone();
        let output_socket_map_clone = Arc::new(output_socket_map);

        tokio::spawn(async move {
            // Create output sockets for each flow
            let mut output_sockets: HashMap<u64, Arc<UdpSocket>> = HashMap::new();

            // Initialize output sockets (bind to any available port, we just need to send)
            for (flow_id, _config) in output_socket_map_clone.iter() {
                let socket = match UdpSocket::bind("0.0.0.0:0").await {
                    Ok(s) => Arc::new(s),
                    Err(_e) => {
                        continue;
                    }
                };
                output_sockets.insert(*flow_id, socket);
            }

            while running_clone.load(Ordering::Relaxed) {
                // Get packets from queue2 (output of EDF) and schedule via output DRR
                if let Ok(packet) = queue2_clone.try_recv() {
                    let _ = output_drr_clone.schedule_packet(packet);
                }

                // Process packets from output DRR and send directly to UDP
                if let Some(packet) = output_drr_clone.process_next() {
                    // Record metrics
                    let latency = packet.timestamp.elapsed();
                    let deadline = packet.timestamp + packet.latency_budget;
                    let deadline_missed = Instant::now() > deadline;
                    metrics_clone.record_packet(
                        packet.flow_id,
                        latency,
                        deadline_missed,
                        packet.latency_budget,
                    );

                    // Route to appropriate output socket based on flow_id and send directly
                    // Use pre-computed SocketAddr to avoid string formatting in hot path
                    if let Some(target_addr) = output_socket_map_clone.get(&packet.flow_id) {
                        if let Some(socket) = output_sockets.get(&packet.flow_id) {
                            let _ = socket.send_to(&packet.data, target_addr).await;
                        }
                    }
                } else {
                    // No packets available - yield to other tasks
                    tokio::task::yield_now().await;
                }
            }
        });

        // Spawn a thread to periodically send metrics updates (even when no packets are processed)
        // This ensures the GUI always has current data
        // Use lower priority thread for statistics to not interfere with packet processing
        let metrics_collector_periodic = self.metrics_collector.clone();
        let running_periodic = self.running.clone();
        let flow_id_to_expected_latency: HashMap<u64, Duration> = self
            .output_sockets
            .iter()
            .map(|config| (config.flow_id, config.latency_budget))
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
