use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Priority levels for packet scheduling
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    HIGH = 0,   // Highest priority (Flow 1)
    MEDIUM = 1, // Medium priority (Flow 2)
    LOW = 2,    // Low priority (Flow 3)
}

impl Priority {
    /// Map flow_id to priority level
    pub fn from_flow_id(flow_id: u64) -> Priority {
        match flow_id {
            1 => Priority::HIGH,
            2 => Priority::MEDIUM,
            3 => Priority::LOW,
            _ => Priority::LOW, // Default to LOW for unknown flows
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // Old DRR scheduler, kept for reference but not used in new architecture
pub struct Flow {
    #[allow(dead_code)]
    pub id: u64,
    #[allow(dead_code)]
    pub quantum: usize,
    #[allow(dead_code)]
    pub deficit: usize,
    #[allow(dead_code)]
    pub latency_budget: Duration,
    #[allow(dead_code)]
    pub priority: Priority,
}

#[derive(Debug, Clone)]
pub struct Packet {
    pub flow_id: u64,
    pub data: Vec<u8>,
    pub timestamp: Instant,
    pub latency_budget: Duration,
    pub priority: Priority,
}

// Combined DRR state to reduce lock acquisitions
#[allow(dead_code)] // Old DRR scheduler, kept for reference but not used in new architecture
struct DRRState {
    flows: HashMap<u64, Flow>,
    #[allow(dead_code)]
    active_flows: Vec<u64>,
    #[allow(dead_code)]
    current_flow_index: usize,
}

#[allow(dead_code)] // Old DRR scheduler, kept for reference but not used in new architecture
pub struct DRRScheduler {
    state: Arc<Mutex<DRRState>>, // Single lock for all DRR state
    #[allow(dead_code)]
    packet_tx: Sender<Packet>,
    #[allow(dead_code)]
    packet_rx: Arc<Mutex<Option<Receiver<Packet>>>>,
    // Three separate queues, one per priority level
    #[allow(dead_code)]
    high_priority_buffer: Arc<Mutex<Vec<Packet>>>,
    #[allow(dead_code)]
    medium_priority_buffer: Arc<Mutex<Vec<Packet>>>,
    #[allow(dead_code)]
    low_priority_buffer: Arc<Mutex<Vec<Packet>>>,
}

#[allow(dead_code)] // Old DRR scheduler, kept for reference but not used in new architecture
impl DRRScheduler {
    pub fn new(packet_tx: Sender<Packet>) -> Self {
        Self {
            state: Arc::new(Mutex::new(DRRState {
                flows: HashMap::new(),
                active_flows: Vec::new(),
                current_flow_index: 0,
            })),
            packet_tx,
            packet_rx: Arc::new(Mutex::new(None)),
            high_priority_buffer: Arc::new(Mutex::new(Vec::new())),
            medium_priority_buffer: Arc::new(Mutex::new(Vec::new())),
            low_priority_buffer: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn set_receiver(&self, rx: Receiver<Packet>) {
        *self.packet_rx.lock() = Some(rx);
    }

    pub fn add_flow(&self, flow_id: u64, quantum: usize, latency_budget: Duration) {
        let priority = Priority::from_flow_id(flow_id);
        let mut state = self.state.lock();
        state.flows.insert(
            flow_id,
            Flow {
                id: flow_id,
                quantum,
                deficit: 0,
                latency_budget,
                priority,
            },
        );

        if !state.active_flows.contains(&flow_id) {
            state.active_flows.push(flow_id);
        }
    }

    pub fn schedule_packet(
        &self,
        mut packet: Packet,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Set priority based on flow_id
        packet.priority = Priority::from_flow_id(packet.flow_id);

        // Add flow if it doesn't exist
        {
            let state = self.state.lock();
            if !state.flows.contains_key(&packet.flow_id) {
                drop(state);
                self.add_flow(packet.flow_id, 1024, packet.latency_budget);
            }
        }

        // Route packet to appropriate priority queue
        match packet.priority {
            Priority::HIGH => {
                let mut buffer = self.high_priority_buffer.lock();
                const MAX_BUFFER_SIZE: usize = 1000;
                let buffer_len = buffer.len();
                if buffer_len >= MAX_BUFFER_SIZE {
                    buffer.drain(0..(buffer_len - MAX_BUFFER_SIZE + 1));
                }
                buffer.push(packet);
            }
            Priority::MEDIUM => {
                let mut buffer = self.medium_priority_buffer.lock();
                const MAX_BUFFER_SIZE: usize = 1000;
                let buffer_len = buffer.len();
                if buffer_len >= MAX_BUFFER_SIZE {
                    buffer.drain(0..(buffer_len - MAX_BUFFER_SIZE + 1));
                }
                buffer.push(packet);
            }
            Priority::LOW => {
                let mut buffer = self.low_priority_buffer.lock();
                const MAX_BUFFER_SIZE: usize = 1000;
                let buffer_len = buffer.len();
                if buffer_len >= MAX_BUFFER_SIZE {
                    buffer.drain(0..(buffer_len - MAX_BUFFER_SIZE + 1));
                }
                buffer.push(packet);
            }
        }

        Ok(())
    }

    pub fn process_next(&self) -> Option<Packet> {
        // Collect incoming packets from channel and route to priority queues
        {
            let rx_guard = self.packet_rx.lock();
            if let Some(rx) = rx_guard.as_ref() {
                while let Ok(mut packet) = rx.try_recv() {
                    // Set priority based on flow_id
                    packet.priority = Priority::from_flow_id(packet.flow_id);

                    // Route to appropriate priority queue
                    match packet.priority {
                        Priority::HIGH => {
                            let mut buffer = self.high_priority_buffer.lock();
                            const MAX_BUFFER_SIZE: usize = 1000;
                            let buffer_len = buffer.len();
                            if buffer_len >= MAX_BUFFER_SIZE {
                                buffer.drain(0..(buffer_len - MAX_BUFFER_SIZE + 1));
                            }
                            buffer.push(packet);
                        }
                        Priority::MEDIUM => {
                            let mut buffer = self.medium_priority_buffer.lock();
                            const MAX_BUFFER_SIZE: usize = 1000;
                            let buffer_len = buffer.len();
                            if buffer_len >= MAX_BUFFER_SIZE {
                                buffer.drain(0..(buffer_len - MAX_BUFFER_SIZE + 1));
                            }
                            buffer.push(packet);
                        }
                        Priority::LOW => {
                            let mut buffer = self.low_priority_buffer.lock();
                            const MAX_BUFFER_SIZE: usize = 1000;
                            let buffer_len = buffer.len();
                            if buffer_len >= MAX_BUFFER_SIZE {
                                buffer.drain(0..(buffer_len - MAX_BUFFER_SIZE + 1));
                            }
                            buffer.push(packet);
                        }
                    }
                }
            }
        }

        // Process packets in priority order: HIGH -> MEDIUM -> LOW
        // Within each priority, use DRR scheduling

        // Try HIGH priority queue first
        if let Some(packet) = self.process_priority_queue(Priority::HIGH) {
            return Some(packet);
        }

        // Try MEDIUM priority queue
        if let Some(packet) = self.process_priority_queue(Priority::MEDIUM) {
            return Some(packet);
        }

        // Try LOW priority queue
        if let Some(packet) = self.process_priority_queue(Priority::LOW) {
            return Some(packet);
        }

        None
    }

    /// Process packets from a specific priority queue using DRR scheduling
    fn process_priority_queue(&self, priority: Priority) -> Option<Packet> {
        // Get the appropriate buffer for this priority
        let (buffer_ref, active_len) = {
            let state = self.state.lock();
            let active_len = state.active_flows.len();
            drop(state);

            let buffer = match priority {
                Priority::HIGH => &self.high_priority_buffer,
                Priority::MEDIUM => &self.medium_priority_buffer,
                Priority::LOW => &self.low_priority_buffer,
            };
            (buffer, active_len)
        };

        let mut buf = buffer_ref.lock();
        if buf.is_empty() {
            return None;
        }

        // If no active flows, just return first packet from this priority queue
        if active_len == 0 {
            let packet = buf.remove(0);
            drop(buf);
            // Add flow if it doesn't exist
            let state = self.state.lock();
            if !state.flows.contains_key(&packet.flow_id) {
                drop(state);
                self.add_flow(packet.flow_id, 1024, packet.latency_budget);
            }
            return Some(packet);
        }

        let now = std::time::Instant::now();

        // Find packet with earliest deadline within this priority queue
        let best_pos = buf
            .iter()
            .enumerate()
            .min_by_key(|(_, p)| {
                let deadline = p.timestamp + p.latency_budget;
                deadline.saturating_duration_since(now)
            })
            .map(|(pos, _)| pos);

        if let Some(pos) = best_pos {
            let packet = buf.remove(pos);
            drop(buf);

            // Update DRR state for this flow
            let mut state = self.state.lock();
            if let Some(flow) = state.flows.get_mut(&packet.flow_id) {
                flow.deficit += flow.quantum;
                if flow.deficit >= 1 {
                    flow.deficit -= 1;
                }
            }
            state.current_flow_index = (state.current_flow_index + 1) % active_len;
            return Some(packet);
        }

        None
    }

    #[allow(dead_code)]
    pub fn has_packets(&self) -> bool {
        if let Some(rx) = self.packet_rx.lock().as_ref() {
            !rx.is_empty()
        } else {
            false
        }
    }

    #[allow(dead_code)]
    pub fn get_flows(&self) -> HashMap<u64, Flow> {
        self.state.lock().flows.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;

    #[test]
    fn test_drr_scheduler_creation() {
        let (tx, _rx) = unbounded();
        let scheduler = DRRScheduler::new(tx);
        assert!(scheduler.state.lock().flows.is_empty());
    }

    #[test]
    fn test_add_flow() {
        let (tx, _rx) = unbounded();
        let scheduler = DRRScheduler::new(tx);
        scheduler.add_flow(1, 1024, Duration::from_millis(1));

        let state = scheduler.state.lock();
        assert!(state.flows.contains_key(&1));
        assert_eq!(state.flows[&1].quantum, 1024);
    }

    #[test]
    fn test_schedule_packet() {
        let (tx, _rx) = unbounded();
        let scheduler = DRRScheduler::new(tx);

        let packet = Packet {
            flow_id: 1,
            data: vec![1, 2, 3],
            timestamp: Instant::now(),
            latency_budget: Duration::from_millis(1),
            priority: Priority::from_flow_id(1),
        };

        // schedule_packet routes to priority buffer, doesn't send to tx
        scheduler.schedule_packet(packet.clone()).unwrap();

        // Add flow so process_next can work
        scheduler.add_flow(1, 1024, Duration::from_millis(1));

        // Retrieve packet via process_next
        let received = scheduler.process_next().unwrap();
        assert_eq!(received.flow_id, 1);
        assert_eq!(received.data, packet.data);
    }
}
