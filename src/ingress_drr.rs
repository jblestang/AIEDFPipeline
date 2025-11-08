use crate::drr_scheduler::{Packet, Priority, PriorityTable};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::UdpSocket as StdUdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Flow state for DRR scheduling
#[derive(Debug, Clone)]
struct FlowState {
    quantum: usize,
    deficit: usize,
    #[allow(dead_code)]
    latency_budget: Duration,
}

/// Combined state for Ingress DRR to reduce lock acquisitions
#[derive(Debug)]
struct IngressDRRState {
    // Socket configurations keyed by priority (HashMap for O(1) lookup)
    socket_configs: HashMap<Priority, SocketConfig>,
    // DRR state: priority -> FlowState
    flow_states: HashMap<Priority, FlowState>,
    // Active flows list for round-robin
    active_flows: Vec<Priority>,
    // Current flow index for round-robin
    current_flow_index: usize,
}

/// Ingress DRR Scheduler - Reads from UDP sockets using DRR scheduling and routes to priority queues
pub struct IngressDRRScheduler {
    // EDF priority queues (sender per priority)
    priority_queues: PriorityTable<crossbeam_channel::Sender<Packet>>,
    // OPTIMIZATION: Single mutex for all state to reduce lock acquisitions
    state: Arc<Mutex<IngressDRRState>>,
    // Drop counters per priority (lock-free atomics)
    drop_counters: PriorityTable<Arc<AtomicU64>>,
}

#[derive(Debug, Clone)]
pub struct SocketConfig {
    pub socket: Arc<StdUdpSocket>,
    pub latency_budget: Duration,
    #[allow(dead_code)]
    pub quantum: usize,
}

impl IngressDRRScheduler {
    pub fn new(priority_queues: PriorityTable<crossbeam_channel::Sender<Packet>>) -> Self {
        let drop_counters = PriorityTable::from_fn(|_| Arc::new(AtomicU64::new(0)));
        Self {
            priority_queues,
            state: Arc::new(Mutex::new(IngressDRRState {
                socket_configs: HashMap::new(),
                flow_states: HashMap::new(),
                active_flows: Vec::new(),
                current_flow_index: 0,
            })),
            drop_counters,
        }
    }

    /// Get drop counts per priority (for metrics)
    pub fn get_drop_counts(&self) -> PriorityTable<u64> {
        PriorityTable::from_fn(|priority| self.drop_counters[priority].load(Ordering::Relaxed))
    }

    pub fn add_socket(
        &self,
        socket: Arc<StdUdpSocket>,
        priority: Priority,
        latency_budget: Duration,
        quantum: usize,
    ) {
        let config = SocketConfig {
            socket,
            latency_budget,
            quantum,
        };

        // OPTIMIZATION: Single lock acquisition for all state updates
        let mut state = self.state.lock();
        state.socket_configs.insert(priority, config);
        state.flow_states.insert(
            priority,
            FlowState {
                quantum,
                deficit: 0,
                latency_budget,
            },
        );
        if !state.active_flows.contains(&priority) {
            state.active_flows.push(priority);
        }
    }

    /// Read from all sockets using DRR scheduling in a single thread
    pub async fn process_sockets(
        &self,
        running: Arc<AtomicBool>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        while running.load(Ordering::Relaxed) {
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
                let socket_configs_snapshot: HashMap<Priority, (Arc<StdUdpSocket>, Duration)> =
                    state
                        .socket_configs
                        .iter()
                        .map(|(priority, config)| {
                            (*priority, (config.socket.clone(), config.latency_budget))
                        })
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

            // DRR (Deficit Round Robin) algorithm:
            // 1. Process flows in round-robin order (fair scheduling, no prioritization)
            // 2. For each flow: add quantum to deficit, then process packets while deficit > 0
            // 3. Decrement deficit by 1 for each packet read until deficit reaches 0
            // 4. When deficit is 0, move flow to back and go to next flow
            // 5. Deficit carries over to next round (not reset)

            // Process each flow in round-robin order starting from current_index
            for i in 0..active_flows.len() {
                let flow_index = (current_index + i) % active_flows.len();
                if !running.load(Ordering::Relaxed) {
                    break;
                }

                let priority = active_flows[flow_index];

                // Get socket config from snapshot (no lock needed)
                let (socket, latency_budget) = match socket_configs_snapshot.get(&priority) {
                    Some((s, l)) => (s.clone(), *l),
                    None => continue,
                };

                // Add quantum to deficit and retrieve local copy (limits lock scope)
                let mut local_deficit = {
                    let mut state = self.state.lock();
                    if let Some(flow) = state.flow_states.get_mut(&priority) {
                        flow.deficit += flow.quantum;
                        flow.deficit
                    } else {
                        0
                    }
                };

                if local_deficit == 0 {
                    continue;
                }

                loop {
                    if !running.load(Ordering::Relaxed) {
                        break;
                    }

                    if local_deficit == 0 {
                        break;
                    }

                    // Try to read from socket (non-blocking)
                    let mut local_buf = [0u8; 1024];
                    match socket.recv_from(&mut local_buf) {
                        Ok((size, _addr)) if size > 0 => {
                            local_deficit = local_deficit.saturating_sub(1);

                            // Create packet
                            let data = Vec::from(&local_buf[..size]);
                            let packet = Packet {
                                flow_id: priority.flow_id(),
                                data,
                                timestamp: Instant::now(),
                                latency_budget,
                                priority,
                            };

                            // Route to appropriate priority queue (no lock needed)
                            let tx = &self.priority_queues[priority];

                            if tx.try_send(packet).is_ok() {
                                packets_read += 1;
                            } else {
                                // Queue full - track drop and move to next flow
                                self.drop_counters[priority].fetch_add(1, Ordering::Relaxed);
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

                // Write back updated deficit once per flow iteration
                {
                    let mut state = self.state.lock();
                    if let Some(flow) = state.flow_states.get_mut(&priority) {
                        flow.deficit = local_deficit;
                    }
                }
            }

            // Update current_index for next round (move to next flow after processing all flows)
            // Only update at the end of the outer loop, not inside the inner loop
            current_index = (current_index + 1) % active_flows.len();

            // OPTIMIZATION: Single lock acquisition to update current index and no other state
            self.state.lock().current_flow_index = current_index;

            // If no packets were read, yield to avoid busy-waiting
            if packets_read == 0 {
                tokio::task::yield_now().await;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {}
