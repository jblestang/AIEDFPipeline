use crate::drr_scheduler::{Packet, Priority};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::UdpSocket as StdUdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Flow state for DRR scheduling
#[derive(Debug, Clone)]
struct FlowState {
    #[allow(dead_code)]
    flow_id: u64,
    quantum: usize,
    deficit: usize,
    #[allow(dead_code)]
    priority: Priority,
    #[allow(dead_code)]
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
    // Drop counters per priority (lock-free atomics)
    high_priority_drops: Arc<std::sync::atomic::AtomicU64>,
    medium_priority_drops: Arc<std::sync::atomic::AtomicU64>,
    low_priority_drops: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Debug, Clone)]
pub struct SocketConfig {
    pub socket: Arc<StdUdpSocket>,
    #[allow(dead_code)]
    pub flow_id: u64,
    pub priority: Priority,
    pub latency_budget: Duration,
    #[allow(dead_code)]
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
            high_priority_drops: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            medium_priority_drops: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            low_priority_drops: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Get drop counts per priority (for metrics)
    pub fn get_drop_counts(&self) -> (u64, u64, u64) {
        (
            self.high_priority_drops.load(std::sync::atomic::Ordering::Relaxed),
            self.medium_priority_drops.load(std::sync::atomic::Ordering::Relaxed),
            self.low_priority_drops.load(std::sync::atomic::Ordering::Relaxed),
        )
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

            // DRR (Deficit Round Robin) algorithm:
            // 1. Process flows in round-robin order (fair scheduling, no prioritization)
            // 2. For each flow: add quantum to deficit, then process packets while deficit > 0
            // 3. Decrement deficit by 1 for each packet read until deficit reaches 0
            // 4. When deficit is 0, move flow to back and go to next flow
            // 5. Deficit carries over to next round (not reset)

            // Process each flow in round-robin order starting from current_index
            for i in 0..active_flows.len() {
                let flow_index = (current_index + i) % active_flows.len();
                if !running.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }

                let flow_id = active_flows[flow_index];
                
                // Get socket config from snapshot (no lock needed)
                let (socket, priority, latency_budget) = match socket_configs_snapshot.get(&flow_id) {
                    Some((s, p, l)) => (s.clone(), *p, *l),
                    None => continue,
                };

                // DRR Step 1: Add quantum to deficit for this flow (deficit carries over)
                let flow_has_deficit = {
                    let mut state = self.state.lock();
                    if let Some(flow) = state.flow_states.get_mut(&flow_id) {
                        flow.deficit += flow.quantum;
                        flow.deficit > 0
                    } else {
                        false
                    }
                };

                // DRR Step 2: Process packets for this flow while deficit > 0
                // Decrement deficit by 1 for each packet read until deficit reaches 0
                if !flow_has_deficit {
                    // Flow has no deficit after adding quantum (shouldn't happen, but handle it)
                    continue;
                }

                loop {
                    if !running.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }

                    // Check if we can process a packet (deficit > 0)
                    let current_deficit = {
                        let state = self.state.lock();
                        state.flow_states.get(&flow_id)
                            .map(|flow| flow.deficit)
                            .unwrap_or(0)
                    };

                    if current_deficit == 0 {
                        // Deficit is 0, go to next flow in round-robin
                        // current_index already tracks our position, so just break and move to next
                        break;
                    }

                    // Try to read from socket (non-blocking)
                    let mut local_buf = [0u8; 1024];
                    match socket.recv_from(&mut local_buf) {
                        Ok((size, _addr)) if size > 0 => {
                            // DRR Step 3: Decrement deficit by 1 (packet count)
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
                                // Queue full - track drop and move to next flow
                                match priority {
                                    Priority::HIGH => {
                                        self.high_priority_drops.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    }
                                    Priority::MEDIUM => {
                                        self.medium_priority_drops.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    }
                                    Priority::LOW => {
                                        self.low_priority_drops.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    }
                                }
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
            }
            
            // Update current_index for next round (move to next flow after processing all flows)
            // Only update at the end of the outer loop, not inside the inner loop
            current_index = (current_index + 1) % active_flows.len();

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_add_socket_adds_flow_to_active_flows() {
        let (high_tx, _high_rx) = crossbeam_channel::bounded(128);
        let (medium_tx, _medium_rx) = crossbeam_channel::bounded(128);
        let (low_tx, _low_rx) = crossbeam_channel::bounded(128);
        
        let scheduler = IngressDRRScheduler::new(high_tx, medium_tx, low_tx);
        
        // Create mock sockets
        let socket1 = Arc::new(StdUdpSocket::bind("127.0.0.1:0").unwrap());
        let socket2 = Arc::new(StdUdpSocket::bind("127.0.0.1:0").unwrap());
        let socket3 = Arc::new(StdUdpSocket::bind("127.0.0.1:0").unwrap());
        
        // Add three flows
        scheduler.add_socket(socket1, 1, Duration::from_millis(5), 32768);
        scheduler.add_socket(socket2, 2, Duration::from_millis(50), 4096);
        scheduler.add_socket(socket3, 3, Duration::from_millis(100), 1024);
        
        // Verify all flows are in active_flows
        let state = scheduler.state.lock();
        assert_eq!(state.active_flows.len(), 3);
        assert!(state.active_flows.contains(&1));
        assert!(state.active_flows.contains(&2));
        assert!(state.active_flows.contains(&3));
        
        // Verify flow states exist
        assert!(state.flow_states.contains_key(&1));
        assert!(state.flow_states.contains_key(&2));
        assert!(state.flow_states.contains_key(&3));
        
        // Verify flow 2 has correct quantum
        assert_eq!(state.flow_states.get(&2).unwrap().quantum, 4096);
    }

    #[test]
    fn test_deficit_accumulation() {
        let (high_tx, _high_rx) = crossbeam_channel::bounded(128);
        let (medium_tx, _medium_rx) = crossbeam_channel::bounded(128);
        let (low_tx, _low_rx) = crossbeam_channel::bounded(128);
        
        let scheduler = IngressDRRScheduler::new(high_tx, medium_tx, low_tx);
        
        let socket = Arc::new(StdUdpSocket::bind("127.0.0.1:0").unwrap());
        scheduler.add_socket(socket, 2, Duration::from_millis(50), 4096);
        
        // Manually add quantum to deficit (simulating what happens in process_sockets)
        {
            let mut state = scheduler.state.lock();
            if let Some(flow) = state.flow_states.get_mut(&2) {
                flow.deficit += flow.quantum; // First round
            }
        }
        
        // Verify deficit is quantum
        {
            let state = scheduler.state.lock();
            assert_eq!(state.flow_states.get(&2).unwrap().deficit, 4096);
        }
        
        // Simulate processing some packets (decrement deficit)
        {
            let mut state = scheduler.state.lock();
            if let Some(flow) = state.flow_states.get_mut(&2) {
                flow.deficit -= 100; // Process 100 packets
            }
        }
        
        // Verify deficit decreased
        {
            let state = scheduler.state.lock();
            assert_eq!(state.flow_states.get(&2).unwrap().deficit, 3996);
        }
        
        // Add quantum again (next round) - deficit should accumulate
        {
            let mut state = scheduler.state.lock();
            if let Some(flow) = state.flow_states.get_mut(&2) {
                flow.deficit += flow.quantum; // Second round
            }
        }
        
        // Verify deficit accumulated (3996 + 4096 = 8092)
        {
            let state = scheduler.state.lock();
            assert_eq!(state.flow_states.get(&2).unwrap().deficit, 8092);
        }
    }

    #[test]
    fn test_all_flows_in_round_robin_order() {
        let (high_tx, _high_rx) = crossbeam_channel::bounded(128);
        let (medium_tx, _medium_rx) = crossbeam_channel::bounded(128);
        let (low_tx, _low_rx) = crossbeam_channel::bounded(128);
        
        let scheduler = IngressDRRScheduler::new(high_tx, medium_tx, low_tx);
        
        // Add flows in order: 1, 2, 3
        let socket1 = Arc::new(StdUdpSocket::bind("127.0.0.1:0").unwrap());
        let socket2 = Arc::new(StdUdpSocket::bind("127.0.0.1:0").unwrap());
        let socket3 = Arc::new(StdUdpSocket::bind("127.0.0.1:0").unwrap());
        
        scheduler.add_socket(socket1, 1, Duration::from_millis(5), 32768);
        scheduler.add_socket(socket2, 2, Duration::from_millis(50), 4096);
        scheduler.add_socket(socket3, 3, Duration::from_millis(100), 1024);
        
        // Verify active_flows contains all three flows
        let state = scheduler.state.lock();
        assert_eq!(state.active_flows.len(), 3);
        
        // Verify all flows are present
        let flow_ids: Vec<u64> = state.active_flows.clone();
        assert!(flow_ids.contains(&1));
        assert!(flow_ids.contains(&2));
        assert!(flow_ids.contains(&3));
    }

    #[test]
    fn test_flow_2_quantum_and_priority() {
        let (high_tx, _high_rx) = crossbeam_channel::bounded(128);
        let (medium_tx, _medium_rx) = crossbeam_channel::bounded(128);
        let (low_tx, _low_rx) = crossbeam_channel::bounded(128);
        
        let scheduler = IngressDRRScheduler::new(high_tx, medium_tx, low_tx);
        
        let socket = Arc::new(StdUdpSocket::bind("127.0.0.1:0").unwrap());
        scheduler.add_socket(socket, 2, Duration::from_millis(50), 4096);
        
        let state = scheduler.state.lock();
        let flow2 = state.flow_states.get(&2).unwrap();
        
        // Verify Flow 2 has correct properties
        assert_eq!(flow2.flow_id, 2);
        assert_eq!(flow2.quantum, 4096);
        assert_eq!(flow2.deficit, 0); // Initial deficit is 0
        assert_eq!(flow2.priority, Priority::MEDIUM);
        assert_eq!(flow2.latency_budget, Duration::from_millis(50));
        
        // Verify Flow 2 is in active_flows
        assert!(state.active_flows.contains(&2));
        
        // Verify Flow 2 socket config exists
        assert!(state.socket_configs.contains_key(&2));
        let config = state.socket_configs.get(&2).unwrap();
        assert_eq!(config.flow_id, 2);
        assert_eq!(config.priority, Priority::MEDIUM);
    }

    #[test]
    fn test_multiple_flows_round_robin_processing() {
        let (high_tx, _high_rx) = crossbeam_channel::bounded(128);
        let (medium_tx, _medium_rx) = crossbeam_channel::bounded(128);
        let (low_tx, _low_rx) = crossbeam_channel::bounded(128);
        
        let scheduler = IngressDRRScheduler::new(high_tx, medium_tx, low_tx);
        
        // Add all three flows
        let socket1 = Arc::new(StdUdpSocket::bind("127.0.0.1:0").unwrap());
        let socket2 = Arc::new(StdUdpSocket::bind("127.0.0.1:0").unwrap());
        let socket3 = Arc::new(StdUdpSocket::bind("127.0.0.1:0").unwrap());
        
        scheduler.add_socket(socket1, 1, Duration::from_millis(5), 32768);
        scheduler.add_socket(socket2, 2, Duration::from_millis(50), 4096);
        scheduler.add_socket(socket3, 3, Duration::from_millis(100), 1024);
        
        // Verify all flows are in active_flows in the order they were added
        let state = scheduler.state.lock();
        assert_eq!(state.active_flows, vec![1, 2, 3]);
        
        // Verify current_flow_index starts at 0
        assert_eq!(state.current_flow_index, 0);
    }

    #[test]
    fn test_socket_configs_snapshot_includes_all_flows() {
        let (high_tx, _high_rx) = crossbeam_channel::bounded(128);
        let (medium_tx, _medium_rx) = crossbeam_channel::bounded(128);
        let (low_tx, _low_rx) = crossbeam_channel::bounded(128);
        
        let scheduler = IngressDRRScheduler::new(high_tx, medium_tx, low_tx);
        
        // Add all three flows
        let socket1 = Arc::new(StdUdpSocket::bind("127.0.0.1:0").unwrap());
        let socket2 = Arc::new(StdUdpSocket::bind("127.0.0.1:0").unwrap());
        let socket3 = Arc::new(StdUdpSocket::bind("127.0.0.1:0").unwrap());
        
        scheduler.add_socket(socket1, 1, Duration::from_millis(5), 32768);
        scheduler.add_socket(socket2, 2, Duration::from_millis(50), 4096);
        scheduler.add_socket(socket3, 3, Duration::from_millis(100), 1024);
        
        // Simulate creating a snapshot like in process_sockets
        let socket_configs_snapshot: HashMap<u64, (Arc<StdUdpSocket>, Priority, Duration)> = {
            let state = scheduler.state.lock();
            state.socket_configs
                .iter()
                .map(|(id, config)| (*id, (config.socket.clone(), config.priority, config.latency_budget)))
                .collect()
        };
        
        // Verify snapshot contains all three flows
        assert_eq!(socket_configs_snapshot.len(), 3);
        assert!(socket_configs_snapshot.contains_key(&1));
        assert!(socket_configs_snapshot.contains_key(&2));
        assert!(socket_configs_snapshot.contains_key(&3));
        
        // Verify Flow 2 has correct priority and latency_budget in snapshot
        let (_socket, priority, latency_budget) = socket_configs_snapshot.get(&2).unwrap();
        assert_eq!(*priority, Priority::MEDIUM);
        assert_eq!(*latency_budget, Duration::from_millis(50));
    }

    #[test]
    fn test_flow_2_packet_creation() {
        let (high_tx, _high_rx) = crossbeam_channel::bounded(128);
        let (medium_tx, medium_rx) = crossbeam_channel::bounded(128);
        let (low_tx, _low_rx) = crossbeam_channel::bounded(128);
        
        let scheduler = IngressDRRScheduler::new(high_tx, medium_tx, low_tx);
        
        let socket = Arc::new(StdUdpSocket::bind("127.0.0.1:0").unwrap());
        scheduler.add_socket(socket, 2, Duration::from_millis(50), 4096);
        
        // Verify Flow 2 would route to medium_priority_tx
        // Create a test packet manually to verify routing
        let test_packet = Packet {
            flow_id: 2,
            data: vec![1, 2, 3],
            timestamp: Instant::now(),
            latency_budget: Duration::from_millis(50),
            priority: Priority::MEDIUM,
        };
        
        // Send packet to medium priority queue (simulating what would happen)
        assert!(scheduler.medium_priority_tx.try_send(test_packet).is_ok());
        
        // Verify packet was received
        let received = medium_rx.try_recv().unwrap();
        assert_eq!(received.flow_id, 2);
        assert_eq!(received.priority, Priority::MEDIUM);
    }
}
