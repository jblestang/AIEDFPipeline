use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Flow {
    #[allow(dead_code)]
    pub id: u64,
    pub quantum: usize,
    pub deficit: usize,
    #[allow(dead_code)]
    pub latency_budget: Duration,
}

#[derive(Debug, Clone)]
pub struct Packet {
    pub flow_id: u64,
    pub data: Vec<u8>,
    pub timestamp: Instant,
    pub latency_budget: Duration,
}

// Combined DRR state to reduce lock acquisitions
struct DRRState {
    flows: HashMap<u64, Flow>,
    active_flows: Vec<u64>,
    current_flow_index: usize,
}

pub struct DRRScheduler {
    state: Arc<Mutex<DRRState>>, // Single lock for all DRR state
    packet_tx: Sender<Packet>,
    packet_rx: Arc<Mutex<Option<Receiver<Packet>>>>,
    packet_buffer: Arc<Mutex<Vec<Packet>>>, // Buffer for packets that don't match current flow
}

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
            packet_buffer: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn set_receiver(&self, rx: Receiver<Packet>) {
        *self.packet_rx.lock() = Some(rx);
    }

    pub fn add_flow(&self, flow_id: u64, quantum: usize, latency_budget: Duration) {
        let mut state = self.state.lock();
        state.flows.insert(
            flow_id,
            Flow {
                id: flow_id,
                quantum,
                deficit: 0,
                latency_budget,
            },
        );

        if !state.active_flows.contains(&flow_id) {
            state.active_flows.push(flow_id);
        }
    }

    pub fn schedule_packet(
        &self,
        packet: Packet,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Add flow if it doesn't exist
        {
            let state = self.state.lock();
            if !state.flows.contains_key(&packet.flow_id) {
                drop(state);
                self.add_flow(packet.flow_id, 1024, packet.latency_budget);
            }
        }

        // Send packet to output
        self.packet_tx.send(packet)?;
        Ok(())
    }

    pub fn process_next(&self) -> Option<Packet> {
        // CRITICAL: Check for Flow 1 packets FIRST before collecting others
        // This ensures Flow 1 packets are processed immediately without delay
        {
            let rx_guard = self.packet_rx.lock();
            if let Some(rx) = rx_guard.as_ref() {
                // Collect ALL available packets and check for Flow 1 first
                let mut temp_packets = Vec::new();
                let mut found_flow1 = None;
                
                // Collect all packets from channel
                while let Ok(packet) = rx.try_recv() {
                    if packet.flow_id == 1 && found_flow1.is_none() {
                        // Found first Flow 1 packet - keep it, collect rest
                        found_flow1 = Some(packet);
                    } else {
                        temp_packets.push(packet);
                    }
                }
                
                // If we found Flow 1, return it immediately and add others to buffer
                if let Some(flow1_packet) = found_flow1 {
                    // Add non-Flow1 packets to buffer for later processing
                    let mut buffer = self.packet_buffer.lock();
                    buffer.extend(temp_packets);
                    // Limit buffer size to prevent unbounded growth (drop oldest if needed)
                    const MAX_BUFFER_SIZE: usize = 1000;
                    let buffer_len = buffer.len();
                    if buffer_len > MAX_BUFFER_SIZE {
                        buffer.drain(0..(buffer_len - MAX_BUFFER_SIZE));
                    }
                    drop(buffer);
                    drop(rx_guard);
                    
                    // Update DRR state and return Flow 1 immediately
                    let mut state = self.state.lock();
                    if let Some(flow) = state.flows.get_mut(&flow1_packet.flow_id) {
                        flow.deficit += flow.quantum;
                        if flow.deficit >= 1 {
                            flow.deficit -= 1;
                        }
                    }
                    return Some(flow1_packet);
                }
                
                // No Flow 1 found, add all collected packets to buffer
                if !temp_packets.is_empty() {
                    let mut buffer = self.packet_buffer.lock();
                    buffer.extend(temp_packets);
                    // Limit buffer size to prevent unbounded growth (drop oldest if needed)
                    const MAX_BUFFER_SIZE: usize = 1000;
                    let buffer_len = buffer.len();
                    if buffer_len > MAX_BUFFER_SIZE {
                        buffer.drain(0..(buffer_len - MAX_BUFFER_SIZE));
                    }
                }
            }
        }

        let mut state = self.state.lock();

        if state.active_flows.is_empty() {
            // If no active flows but we have packets, prioritize Flow 1 first, then by time remaining
            drop(state); // Release state lock before buffer operations
            let mut buffer = self.packet_buffer.lock();
            if !buffer.is_empty() {
                // First, check if there's a Flow 1 packet (always highest priority)
                if let Some(flow1_pos) = buffer.iter().position(|p| p.flow_id == 1) {
                    let packet = buffer.remove(flow1_pos);
                    drop(buffer);
                    // Add flow if it doesn't exist
                    let mut state = self.state.lock();
                    if !state.flows.contains_key(&packet.flow_id) {
                        drop(state);
                        self.add_flow(packet.flow_id, 1024, packet.latency_budget);
                    }
                    return Some(packet);
                }
                
                // If no Flow 1, find packet with least time remaining until deadline
                let now = std::time::Instant::now();
                let best_pos = buffer
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, p)| {
                        let deadline = p.timestamp + p.latency_budget;
                        deadline.saturating_duration_since(now)
                    })
                    .map(|(pos, _)| pos);
                
                if let Some(pos) = best_pos {
                    let packet = buffer.remove(pos);
                    drop(buffer);
                    // Add flow if it doesn't exist
                    let mut state = self.state.lock();
                    if !state.flows.contains_key(&packet.flow_id) {
                        drop(state);
                        self.add_flow(packet.flow_id, 1024, packet.latency_budget);
                    }
                    return Some(packet);
                }
            }
            return None;
        }

        let mut attempts = 0;
        let max_attempts = state.active_flows.len();

        // Priority: Always prioritize Flow 1 first, then by time remaining until deadline
        // Release state lock before buffer operations to minimize lock duration
        let active_len = state.active_flows.len();
        drop(state);
        
        let mut buffer = self.packet_buffer.lock();
        let now = std::time::Instant::now();
        
        // Optimize: Single pass to find Flow 1 or best deadline packet
        // This avoids two separate iterations over the buffer
        let mut flow1_pos: Option<usize> = None;
        let mut best_pos: Option<(usize, Duration)> = None;
        
        for (idx, p) in buffer.iter().enumerate() {
            // Check for Flow 1 (highest priority)
            if p.flow_id == 1 {
                flow1_pos = Some(idx);
                break; // Flow 1 found, no need to continue
            }
            
            // Track best deadline if no Flow 1 yet
            if flow1_pos.is_none() {
                let deadline = p.timestamp + p.latency_budget;
                let time_remaining = deadline.saturating_duration_since(now);
                if best_pos.is_none() || time_remaining < best_pos.unwrap().1 {
                    best_pos = Some((idx, time_remaining));
                }
            }
        }
        
        // Process Flow 1 if found, otherwise use best deadline
        let selected_pos = flow1_pos.or_else(|| best_pos.map(|(pos, _)| pos));
        
        if let Some(pos) = selected_pos {
            let packet = buffer.remove(pos);
            drop(buffer);
            
            // Update DRR state for this flow (brief lock)
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
        drop(buffer);

        // Try to find a packet for the current flow (normal DRR)
        while attempts < max_attempts {
            // Get current flow_id and index (brief lock)
            let (flow_id, current_idx, active_len) = {
                let state = self.state.lock();
                if state.active_flows.is_empty() {
                    break;
                }
                let idx = state.current_flow_index % state.active_flows.len();
                let fid = state.active_flows[idx];
                (fid, idx, state.active_flows.len())
            };

            // Find a packet for this flow in the buffer (no state lock held)
            let mut buffer = self.packet_buffer.lock();
            if let Some(pos) = buffer.iter().position(|p| p.flow_id == flow_id) {
                let packet = buffer.remove(pos);
                drop(buffer);

                // Update DRR state (brief lock)
                let mut state = self.state.lock();
                if let Some(flow) = state.flows.get_mut(&flow_id) {
                    flow.deficit += flow.quantum;
                    if flow.deficit >= 1 {
                        flow.deficit -= 1;
                    } else {
                        flow.deficit = 0;
                    }
                }
                state.current_flow_index = (current_idx + 1) % active_len;
                return Some(packet);
            } else {
                drop(buffer);
                // Update index for next attempt (brief lock)
                let mut state = self.state.lock();
                state.current_flow_index = (current_idx + 1) % active_len;
            }
            attempts += 1;
        }

        // If we have packets but none matched, prioritize Flow 1 first, then by time remaining
        let mut buffer = self.packet_buffer.lock();
        if !buffer.is_empty() {
            // First, check if there's a Flow 1 packet (always highest priority)
            if let Some(flow1_pos) = buffer.iter().position(|p| p.flow_id == 1) {
                let packet = buffer.remove(flow1_pos);
                drop(buffer);

                // Add flow if it doesn't exist and update state (brief lock)
                let mut state = self.state.lock();
                if !state.flows.contains_key(&packet.flow_id) {
                    drop(state);
                    self.add_flow(packet.flow_id, 1024, packet.latency_budget);
                    let mut state = self.state.lock();
                    if let Some(flow) = state.flows.get_mut(&packet.flow_id) {
                        flow.deficit += flow.quantum;
                        if flow.deficit >= 1 {
                            flow.deficit -= 1;
                        } else {
                            flow.deficit = 0;
                        }
                    }
                    return Some(packet);
                }
                if let Some(flow) = state.flows.get_mut(&packet.flow_id) {
                    flow.deficit += flow.quantum;
                    if flow.deficit >= 1 {
                        flow.deficit -= 1;
                    } else {
                        flow.deficit = 0;
                    }
                }
                return Some(packet);
            }
            
            // If no Flow 1, find packet with least time remaining until deadline
            let now = std::time::Instant::now();
            let best_pos = buffer
                .iter()
                .enumerate()
                .min_by_key(|(_, p)| {
                    let deadline = p.timestamp + p.latency_budget;
                    deadline.saturating_duration_since(now)
                })
                .map(|(pos, _)| pos);
            
            if let Some(pos) = best_pos {
                let packet = buffer.remove(pos);
                drop(buffer);

                // Add flow if it doesn't exist and update state (brief lock)
                let mut state = self.state.lock();
                if !state.flows.contains_key(&packet.flow_id) {
                    drop(state);
                    self.add_flow(packet.flow_id, 1024, packet.latency_budget);
                    let mut state = self.state.lock();
                    if let Some(flow) = state.flows.get_mut(&packet.flow_id) {
                        flow.deficit += flow.quantum;
                        if flow.deficit >= 1 {
                            flow.deficit -= 1;
                        } else {
                            flow.deficit = 0;
                        }
                    }
                    return Some(packet);
                }
                if let Some(flow) = state.flows.get_mut(&packet.flow_id) {
                    flow.deficit += flow.quantum;
                    if flow.deficit >= 1 {
                        flow.deficit -= 1;
                    } else {
                        flow.deficit = 0;
                    }
                }
                return Some(packet);
            }
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
        let (tx, rx) = unbounded();
        let scheduler = DRRScheduler::new(tx);

        let packet = Packet {
            flow_id: 1,
            data: vec![1, 2, 3],
            timestamp: Instant::now(),
            latency_budget: Duration::from_millis(1),
        };

        scheduler.schedule_packet(packet.clone()).unwrap();
        let received = rx.recv().unwrap();
        assert_eq!(received.flow_id, 1);
    }
}
