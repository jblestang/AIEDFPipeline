//! Ingress Deficit Round Robin scheduler.
//!
//! A single Tokio runtime polls non-blocking UDP sockets, applies packet-count based DRR, and
//! forwards packets into per-priority bounded channels destined for the EDF processor.

use crate::drr_scheduler::{Packet, Priority, PriorityTable, MAX_PACKET_SIZE};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::UdpSocket as StdUdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// DRR state tracked per priority class.
#[derive(Debug)]
struct FlowState {
    /// Number of packets the flow can emit in the current round.
    quantum: usize,
    /// Remaining credit within the current round (updated atomically).
    deficit: AtomicUsize,
    /// Latency budget used when instantiating [`Packet`] objects.
    latency_budget: Duration,
}

/// Snapshot used during processing to avoid holding the state lock in the hot loop.
#[derive(Clone)]
struct FlowSnapshot {
    socket: Arc<StdUdpSocket>,
    state: Arc<FlowState>,
}

/// Combined state guarded by a single mutex so we can minimise lock churn while iterating flows.
#[derive(Debug)]
struct IngressDRRState {
    /// Socket configuration keyed by priority for O(1) lookup.
    socket_configs: HashMap<Priority, SocketConfig>,
    /// Per-priority DRR state (quantum + deficit atomics).
    flow_states: HashMap<Priority, Arc<FlowState>>,
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
        let config = SocketConfig { socket };

        // OPTIMISATION: Build per-flow state once and store atomics for deficit updates
        let flow_state = Arc::new(FlowState {
            quantum,
            deficit: AtomicUsize::new(0),
            latency_budget,
        });

        // OPTIMIZATION: Single lock acquisition for all state updates
        let mut state = self.state.lock();
        state.socket_configs.insert(priority, config);
        state.flow_states.insert(priority, flow_state);
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
            let (active_flows, current_index, snapshot_map) = {
                let state = self.state.lock();
                let is_empty = state.active_flows.is_empty();
                let active_flows = if is_empty {
                    Vec::new()
                } else {
                    state.active_flows.clone()
                };
                let current_index = state.current_flow_index;
                let snapshot_map: HashMap<Priority, FlowSnapshot> = state
                    .active_flows
                    .iter()
                    .filter_map(|priority| {
                        let socket = state.socket_configs.get(priority)?;
                        let flow_state = state.flow_states.get(priority)?;
                        Some((
                            *priority,
                            FlowSnapshot {
                                socket: socket.socket.clone(),
                                state: flow_state.clone(),
                            },
                        ))
                    })
                    .collect();
                drop(state); // Explicitly drop before await
                (active_flows, current_index, snapshot_map)
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

                // Get socket and flow state from snapshot (no lock needed)
                let flow_snapshot = match snapshot_map.get(&priority) {
                    Some(snapshot) => snapshot.clone(),
                    None => continue,
                };
                let socket = flow_snapshot.socket;
                let flow_state = flow_snapshot.state;

                // Atomically add quantum to deficit and retrieve local copy
                let mut local_deficit = flow_state
                    .deficit
                    .fetch_add(flow_state.quantum, Ordering::Relaxed)
                    + flow_state.quantum;

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
                    let mut local_buf = [0u8; MAX_PACKET_SIZE];
                    match socket.recv_from(&mut local_buf) {
                        Ok((size, _addr)) if size > 0 => {
                            local_deficit = local_deficit.saturating_sub(1);

                            // Materialise packet (including timestamp used for EDF deadlines).
                            let packet = Packet::new(
                                priority,
                                &local_buf[..size],
                                flow_state.latency_budget,
                            );

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

                // Persist updated deficit for the next round (no lock required)
                flow_state.deficit.store(local_deficit, Ordering::Relaxed);
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
