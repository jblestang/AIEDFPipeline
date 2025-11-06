use crate::drr_scheduler::{Packet, Priority};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::UdpSocket as StdUdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Flow state for DRR scheduling
#[derive(Debug, Clone)]
struct FlowState {
    flow_id: u64,
    quantum: usize,
    deficit: usize,
    priority: Priority,
    latency_budget: Duration,
}

/// Ingress DRR Scheduler - Reads from UDP sockets using DRR scheduling and routes to priority queues
pub struct IngressDRRScheduler {
    // Three priority queues for EDF input
    high_priority_tx: crossbeam_channel::Sender<Packet>,
    medium_priority_tx: crossbeam_channel::Sender<Packet>,
    low_priority_tx: crossbeam_channel::Sender<Packet>,
    // Socket configurations with priority mapping
    socket_configs: Arc<Mutex<Vec<SocketConfig>>>,
    // DRR state: flow_id -> FlowState
    flow_states: Arc<Mutex<HashMap<u64, FlowState>>>,
    // Active flows list for round-robin
    active_flows: Arc<Mutex<Vec<u64>>>,
    // Current flow index for round-robin
    current_flow_index: Arc<Mutex<usize>>,
}

#[derive(Debug, Clone)]
pub struct SocketConfig {
    pub socket: Arc<StdUdpSocket>,
    pub flow_id: u64,
    pub priority: Priority,
    pub latency_budget: Duration,
    pub quantum: usize,
}

impl IngressDRRScheduler {
    pub fn new(
        high_priority_tx: crossbeam_channel::Sender<Packet>,
        medium_priority_tx: crossbeam_channel::Sender<Packet>,
        low_priority_tx: crossbeam_channel::Sender<Packet>,
    ) -> Self {
        Self {
            high_priority_tx,
            medium_priority_tx,
            low_priority_tx,
            socket_configs: Arc::new(Mutex::new(Vec::new())),
            flow_states: Arc::new(Mutex::new(HashMap::new())),
            active_flows: Arc::new(Mutex::new(Vec::new())),
            current_flow_index: Arc::new(Mutex::new(0)),
        }
    }

    pub fn add_socket(
        &self,
        socket: Arc<StdUdpSocket>,
        flow_id: u64,
        latency_budget: Duration,
        quantum: usize,
    ) {
        let priority = Priority::from_flow_id(flow_id);
        let config = SocketConfig {
            socket,
            flow_id,
            priority,
            latency_budget,
            quantum,
        };
        self.socket_configs.lock().push(config.clone());

        // Initialize flow state for DRR
        let mut flow_states = self.flow_states.lock();
        flow_states.insert(
            flow_id,
            FlowState {
                flow_id,
                quantum,
                deficit: 0,
                priority,
                latency_budget,
            },
        );
        drop(flow_states);

        let mut active_flows = self.active_flows.lock();
        if !active_flows.contains(&flow_id) {
            active_flows.push(flow_id);
        }
    }

    /// Read from all sockets using DRR scheduling in a single thread
    pub async fn process_sockets(
        &self,
        running: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        while running.load(std::sync::atomic::Ordering::Relaxed) {
            let active_flows = self.active_flows.lock().clone();
            drop(self.active_flows.lock());

            if active_flows.is_empty() {
                tokio::task::yield_now().await;
                continue;
            }

            // Get current flow index and iterate through active flows
            let mut current_index = *self.current_flow_index.lock();
            let mut packets_read = 0;
            let max_iterations = active_flows.len();

            for _ in 0..max_iterations {
                if !running.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }

                let flow_id = active_flows[current_index];
                current_index = (current_index + 1) % active_flows.len();

                // Get socket config
                let (socket, priority, latency_budget) = {
                    let socket_configs = self.socket_configs.lock();
                    let config = match socket_configs.iter().find(|c| c.flow_id == flow_id) {
                        Some(c) => c,
                        None => continue,
                    };
                    (
                        config.socket.clone(),
                        config.priority,
                        config.latency_budget,
                    )
                };

                // Update deficit: add quantum
                let mut flow_states = self.flow_states.lock();
                let flow = match flow_states.get_mut(&flow_id) {
                    Some(f) => f,
                    None => continue,
                };
                flow.deficit += flow.quantum;

                // Check if we can read a packet (deficit >= 1)
                if flow.deficit >= 1 {
                    drop(flow_states);

                    // Try to read from socket (non-blocking)
                    let mut local_buf = [0u8; 1024];
                    match socket.recv_from(&mut local_buf) {
                        Ok((size, _addr)) => {
                            if size > 0 {
                                // Decrement deficit by 1 (packet count)
                                let mut flow_states = self.flow_states.lock();
                                if let Some(flow) = flow_states.get_mut(&flow_id) {
                                    flow.deficit -= 1;
                                }
                                drop(flow_states);

                                // Create packet
                                let data = Vec::from(&local_buf[..size]);
                                let packet = Packet {
                                    flow_id,
                                    data,
                                    timestamp: Instant::now(),
                                    latency_budget,
                                    priority,
                                };

                                // Route to appropriate priority queue
                                let tx = match priority {
                                    Priority::HIGH => &self.high_priority_tx,
                                    Priority::MEDIUM => &self.medium_priority_tx,
                                    Priority::LOW => &self.low_priority_tx,
                                };

                                if tx.try_send(packet).is_err() {
                                    // Queue full - packet dropped
                                }

                                packets_read += 1;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            // No packet available, continue to next flow
                        }
                        Err(_) => {
                            // Socket error, continue to next flow
                        }
                    }
                } else {
                    drop(flow_states);
                }
            }

            // Update current flow index
            *self.current_flow_index.lock() = current_index;

            // If no packets were read, yield to avoid busy-waiting
            if packets_read == 0 {
                tokio::task::yield_now().await;
            }
        }

        Ok(())
    }
}
