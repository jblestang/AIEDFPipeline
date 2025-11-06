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

            // DRR (Deficit Round Robin) algorithm:
            // 1. Process flows in round-robin order (prioritize Flow 1 first if it exists)
            // 2. For each flow: add quantum to deficit, then process packets until deficit < 1
            // 3. Move to next flow in round-robin
            // 4. Deficit carries over to next round (not reset)
            
            // Build processing order: Flow 1 first (if exists), then others in round-robin
            let flow_1_index = active_flows.iter().position(|&id| id == 1);
            let mut processing_order: Vec<usize> = Vec::with_capacity(active_flows.len());
            
            // Add Flow 1 first if it exists
            if let Some(idx) = flow_1_index {
                processing_order.push(idx);
            }
            
            // Add other flows in round-robin order starting from current_index
            for i in 0..active_flows.len() {
                let idx = (current_index + i) % active_flows.len();
                if flow_1_index.map_or(true, |flow_1_idx| idx != flow_1_idx) {
                    processing_order.push(idx);
                }
            }

            // Process each flow in order using DRR
            for &flow_index in &processing_order {
                if !running.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }

                let flow_id = active_flows[flow_index];
                
                // Get socket config from snapshot (no lock needed)
                let (socket, priority, latency_budget) = match socket_configs_snapshot.get(&flow_id) {
                    Some((s, p, l)) => (s.clone(), *p, *l),
                    None => {
                        current_index = (flow_index + 1) % active_flows.len();
                        continue;
                    }
                };

                // DRR Step 1: Add quantum to deficit for this flow
                {
                    let mut state = self.state.lock();
                    if let Some(flow) = state.flow_states.get_mut(&flow_id) {
                        flow.deficit += flow.quantum;
                    }
                }

                // DRR Step 2: Process packets for this flow until deficit < 1 (packet cost = 1)
                // This allows the flow to send up to 'quantum' packets per round
                loop {
                    if !running.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }

                    // Check if we can process a packet (deficit >= 1)
                    let can_process = {
                        let state = self.state.lock();
                        state.flow_states.get(&flow_id)
                            .map(|flow| flow.deficit >= 1)
                            .unwrap_or(false)
                    };

                    if !can_process {
                        // Deficit exhausted for this flow, move to next flow in round-robin
                        break;
                    }

                    // Try to read from socket (non-blocking)
                    let mut local_buf = [0u8; 1024];
                    match socket.recv_from(&mut local_buf) {
                        Ok((size, _addr)) if size > 0 => {
                            // DRR Step 3: Decrement deficit by 1 (packet count, not packet length)
                            {
                                let mut state = self.state.lock();
                                if let Some(flow) = state.flow_states.get_mut(&flow_id) {
                                    flow.deficit -= 1;
                                }
                            }
                            
                            // Create packet
                            let data = Vec::from(&local_buf[..size]);
                            let packet = Packet {
                                flow_id,
                                data,
                                timestamp: Instant::now(),
                                latency_budget,
                                priority,
                            };

                            // Route to appropriate priority queue (no lock needed)
                            let tx = match priority {
                                Priority::HIGH => &self.high_priority_tx,
                                Priority::MEDIUM => &self.medium_priority_tx,
                                Priority::LOW => &self.low_priority_tx,
                            };

                            if tx.try_send(packet).is_ok() {
                                packets_read += 1;
                            } else {
                                // Queue full, move to next flow
                                break;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            // No packet available for this flow, move to next flow
                            break;
                        }
                        Err(_) => {
                            // Socket error, move to next flow
                            break;
                        }
                        _ => break,
                    }
                }

                // Update current_index for next round
                current_index = (flow_index + 1) % active_flows.len();
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
