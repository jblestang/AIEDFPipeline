//! Ingress Deficit Round Robin scheduler.
//! 
//! A single Tokio runtime polls non-blocking UDP sockets, applies packet-count based DRR, and
//! forwards packets into per-priority bounded channels destined for the EDF processor.

use crate::drr_scheduler::{Packet, Priority, PriorityTable};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::UdpSocket as StdUdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// DRR state tracked per priority class.
#[derive(Debug, Clone)]
struct FlowState {
    /// Number of packets the flow can emit in the current round.
    quantum: usize,
    /// Remaining credit within the current round.
    deficit: usize,
    /// Latency budget used when instantiating [`Packet`] objects.
    #[allow(dead_code)]
    latency_budget: Duration,
}

/// Combined state guarded by a single mutex so we can minimise lock churn while iterating flows.
#[derive(Debug)]
struct IngressDRRState {
    /// Socket configuration keyed by priority for O(1) lookup.
    socket_configs: HashMap<Priority, SocketConfig>,
    /// Per-priority DRR state (quantum + deficit).
    flow_states: HashMap<Priority, FlowState>,
    /// Ordered list of active flows used by the round-robin iterator.
    active_flows: Vec<Priority>,
    /// Index within `active_flows` that will be serviced next.
    current_flow_index: usize,
}

/// Ingress DRR scheduler that enforces packet-count fairness before handing packets to EDF.
pub struct IngressDRRScheduler {
    /// Crossbeam senders for each priority class.
    priority_queues: PriorityTable<crossbeam_channel::Sender<Packet>>,
    /// Shared mutable state for flow metadata.
    state: Arc<Mutex<IngressDRRState>>,
    /// Drop counter per priority exposed via metrics.
    drop_counters: PriorityTable<Arc<AtomicU64>>,
}

/// Configuration captured when adding an input socket.
#[derive(Debug, Clone)]
pub struct SocketConfig {
    pub socket: Arc<StdUdpSocket>,
    pub latency_budget: Duration,
    #[allow(dead_code)]
    pub quantum: usize,
}

impl IngressDRRScheduler {
    /// Create a new ingress DRR scheduler bound to the provided priority queues.
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

    /// Return per-priority ingress drop counters for metrics emission.
    pub fn get_drop_counts(&self) -> PriorityTable<u64> {
        PriorityTable::from_fn(|priority| self.drop_counters[priority].load(Ordering::Relaxed))
    }

    /// Register a new input socket associated with a priority class and DRR quantum.
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

        // Single lock acquisition captures socket details, DRR state, and active flow tracking.
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

    /// Drive all sockets with packet-count DRR until shutdown.
    pub async fn process_sockets(
        &self,
        running: Arc<AtomicBool>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        while running.load(Ordering::Relaxed) {
            // Snapshot shared state outside the hot loop so we keep the mutex hold time short.
            let (active_flows, current_index, socket_configs_snapshot) = {
                let state = self.state.lock();
                let is_empty = state.active_flows.is_empty();
                let active_flows = if is_empty {
                    Vec::new()
                } else {
                    state.active_flows.clone()
                };
                let current_index = state.current_flow_index;
                // Snapshot the sockets (Arc clones) so the inner loop does not lock.
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

            // Iterate through flows in round-robin order and honour the per-priority deficit.
            for i in 0..active_flows.len() {
                let flow_index = (current_index + i) % active_flows.len();
                if !running.load(Ordering::Relaxed) {
                    break;
                }

                let priority = active_flows[flow_index];

                // Look up socket and latency budget using the immutable snapshot.
                let (socket, latency_budget) = match socket_configs_snapshot.get(&priority) {
                    Some((s, l)) => (s.clone(), *l),
                    None => continue,
                };

                // Increment deficit inside a short lock scope.
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

                    // Non-blocking read from UDP socket.
                    let mut local_buf = [0u8; 1024];
                    match socket.recv_from(&mut local_buf) {
                        Ok((size, _addr)) if size > 0 => {
                            local_deficit = local_deficit.saturating_sub(1);

                            // Materialise packet (including timestamp used for EDF deadlines).
                            let data = Vec::from(&local_buf[..size]);
                            let packet = Packet {
                                flow_id: priority.flow_id(),
                                data,
                                timestamp: Instant::now(),
                                latency_budget,
                                priority,
                            };

                            let tx = &self.priority_queues[priority];

                            if tx.try_send(packet).is_ok() {
                                packets_read += 1;
                            } else {
                                // Channel full → drop packet and move to the next flow.
                                self.drop_counters[priority].fetch_add(1, Ordering::Relaxed);
                                break;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            // No data for this flow yet.
                            break;
                        }
                        Err(_) => {
                            // Socket error; skip to next flow.
                            break;
                        }
                        _ => break,
                    }
                }

                // Persist updated deficit for the next round.
                {
                    let mut state = self.state.lock();
                    if let Some(flow) = state.flow_states.get_mut(&priority) {
                        flow.deficit = local_deficit;
                    }
                }
            }

            // Advance the round-robin pointer once we serviced everyone.
            current_index = (current_index + 1) % active_flows.len();

            self.state.lock().current_flow_index = current_index;

            if packets_read == 0 {
                tokio::task::yield_now().await;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {}
