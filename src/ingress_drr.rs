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

/// Combined state for Ingress DRR to reduce lock acquisitions
#[derive(Debug)]
struct IngressDRRState {
    // Socket configurations: flow_id -> SocketConfig (HashMap for O(1) lookup)
    socket_configs: HashMap<u64, SocketConfig>,
    // DRR state: flow_id -> FlowState
    flow_states: HashMap<u64, FlowState>,
    // Active flows list for round-robin
    active_flows: Vec<u64>,
    // Current flow index for round-robin
    current_flow_index: usize,
}

/// Ingress DRR Scheduler - Reads from UDP sockets using DRR scheduling and routes to priority queues
pub struct IngressDRRScheduler {
    // Three priority queues for EDF input
    high_priority_tx: crossbeam_channel::Sender<Packet>,
    medium_priority_tx: crossbeam_channel::Sender<Packet>,
    low_priority_tx: crossbeam_channel::Sender<Packet>,
    // OPTIMIZATION: Single mutex for all state to reduce lock acquisitions
    state: Arc<Mutex<IngressDRRState>>,
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
            state: Arc::new(Mutex::new(IngressDRRState {
                socket_configs: HashMap::new(),
                flow_states: HashMap::new(),
                active_flows: Vec::new(),
                current_flow_index: 0,
            })),
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
        
        // OPTIMIZATION: Single lock acquisition for all state updates
        let mut state = self.state.lock();
        state.socket_configs.insert(flow_id, config);
        state.flow_states.insert(
            flow_id,
            FlowState {
                flow_id,
                quantum,
                deficit: 0,
                priority,
                latency_budget,
            },
        );
        if !state.active_flows.contains(&flow_id) {
            state.active_flows.push(flow_id);
        }
    }

    /// Read from all sockets using DRR scheduling in a single thread
    pub async fn process_sockets(
        &self,
        running: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        while running.load(std::sync::atomic::Ordering::Relaxed) {
            // OPTIMIZATION: Single lock acquisition to get all needed state
            let (active_flows, current_index, socket_configs_snapshot) = {
                let state = self.state.lock();
                let is_empty = state.active_flows.is_empty();
                let active_flows = if is_empty {
                    Vec::new()
                } else {
                    state.active_flows.clone()
                };
                let current_index = state.current_flow_index;
                // Create snapshot of socket configs (only socket, priority, latency_budget)
                let socket_configs_snapshot: HashMap<u64, (Arc<StdUdpSocket>, Priority, Duration)> = state
                    .socket_configs
                    .iter()
                    .map(|(id, config)| (*id, (config.socket.clone(), config.priority, config.latency_budget)))
                    .collect();
                drop(state); // Explicitly drop before await
                (active_flows, current_index, socket_configs_snapshot)
            };
            
            if active_flows.is_empty() {
                tokio::task::yield_now().await;
                continue;
            }

            let mut current_index = current_index;
            let mut packets_read = 0;
            let max_iterations = active_flows.len();

            // OPTIMIZATION: Prioritize Flow 1 (HIGH priority) by processing it first and exhaustively
            // Process Flow 1 until deficit is exhausted or no packets available
            let flow_1_index = active_flows.iter().position(|&id| id == 1);
            
            if let Some(_flow_1_idx) = flow_1_index {
                let flow_id = 1;
                // Get socket config from snapshot (no lock needed)
                if let Some((socket, priority, latency_budget)) = socket_configs_snapshot.get(&flow_id) {
                    let socket = socket.clone();
                    let priority = *priority;
                    let latency_budget = *latency_budget;

                    // Add quantum at the start of Flow 1 processing
                    {
                        let mut state = self.state.lock();
                        if let Some(flow) = state.flow_states.get_mut(&flow_id) {
                            flow.deficit += flow.quantum;
                        }
                    }

                    // Process Flow 1 exhaustively until deficit is exhausted or no packets available
                    loop {
                        if !running.load(std::sync::atomic::Ordering::Relaxed) {
                            break;
                        }

                        let mut local_buf = [0u8; 1024];
                        let packet_result = {
                            let mut state = self.state.lock();
                            if let Some(flow) = state.flow_states.get_mut(&flow_id) {
                                // Check if we can read a packet (deficit >= 1)
                                if flow.deficit >= 1 {
                                    // Release lock before socket read (non-blocking)
                                    drop(state);
                                    
                                    // Try to read from socket (non-blocking)
                                    match socket.recv_from(&mut local_buf) {
                                        Ok((size, _addr)) if size > 0 => {
                                            // Re-acquire lock to update deficit
                                            let mut state = self.state.lock();
                                            if let Some(flow) = state.flow_states.get_mut(&flow_id) {
                                                flow.deficit -= 1;
                                            }
                                            drop(state);
                                            
                                            // Create packet
                                            let data = Vec::from(&local_buf[..size]);
                                            Some(Packet {
                                                flow_id,
                                                data,
                                                timestamp: Instant::now(),
                                                latency_budget,
                                                priority,
                                            })
                                        }
                                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                            // No packet available, break out of Flow 1 loop
                                            break;
                                        }
                                        Err(_) => {
                                            // Socket error, break out of Flow 1 loop
                                            break;
                                        }
                                        _ => None,
                                    }
                                } else {
                                    // Deficit exhausted, break out of Flow 1 loop
                                    break;
                                }
                            } else {
                                break;
                            }
                        };

                        if let Some(packet) = packet_result {
                            // Route to appropriate priority queue (no lock needed)
                            let tx = match priority {
                                Priority::HIGH => &self.high_priority_tx,
                                Priority::MEDIUM => &self.medium_priority_tx,
                                Priority::LOW => &self.low_priority_tx,
                            };

                            if tx.try_send(packet).is_ok() {
                                packets_read += 1;
                            } else {
                                // Queue full, break to avoid blocking
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                }
            }

            // Now process other flows in round-robin order
            for _ in 0..max_iterations {
                if !running.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }

                let flow_id = active_flows[current_index];
                // Skip Flow 1 if we already processed it
                if flow_id == 1 && flow_1_index.is_some() {
                    current_index = (current_index + 1) % active_flows.len();
                    continue;
                }
                current_index = (current_index + 1) % active_flows.len();

                // Get socket config from snapshot (no lock needed)
                let (socket, priority, latency_budget) = match socket_configs_snapshot.get(&flow_id) {
                    Some((s, p, l)) => (s.clone(), *p, *l),
                    None => continue,
                };

                // OPTIMIZATION: Single lock acquisition for deficit update and packet read
                let mut local_buf = [0u8; 1024];
                let packet_result = {
                    let mut state = self.state.lock();
                    let flow = match state.flow_states.get_mut(&flow_id) {
                        Some(f) => f,
                        None => continue,
                    };
                    
                    // Update deficit: add quantum
                    flow.deficit += flow.quantum;

                    // Check if we can read a packet (deficit >= 1)
                    if flow.deficit >= 1 {
                        // Release lock before socket read (non-blocking)
                        drop(state);
                        
                        // Try to read from socket (non-blocking)
                        match socket.recv_from(&mut local_buf) {
                            Ok((size, _addr)) if size > 0 => {
                                // Re-acquire lock to update deficit
                                let mut state = self.state.lock();
                                if let Some(flow) = state.flow_states.get_mut(&flow_id) {
                                    flow.deficit -= 1;
                                }
                                drop(state);
                                
                                // Create packet
                                let data = Vec::from(&local_buf[..size]);
                                Some(Packet {
                                    flow_id,
                                    data,
                                    timestamp: Instant::now(),
                                    latency_budget,
                                    priority,
                                })
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => None,
                            Err(_) => None,
                            _ => None,
                        }
                    } else {
                        None
                    }
                };

                if let Some(packet) = packet_result {
                    // Route to appropriate priority queue (no lock needed)
                    let tx = match priority {
                        Priority::HIGH => &self.high_priority_tx,
                        Priority::MEDIUM => &self.medium_priority_tx,
                        Priority::LOW => &self.low_priority_tx,
                    };

                    if tx.try_send(packet).is_ok() {
                        packets_read += 1;
                    }
                }
            }

            // OPTIMIZATION: Single lock acquisition to update current index
            {
                let mut state = self.state.lock();
                state.current_flow_index = current_index;
            }

            // If no packets were read, yield to avoid busy-waiting
            if packets_read == 0 {
                tokio::task::yield_now().await;
            }
        }

        Ok(())
    }
}
