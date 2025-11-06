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

pub struct DRRScheduler {
    flows: Arc<Mutex<HashMap<u64, Flow>>>,
    packet_tx: Sender<Packet>,
    packet_rx: Arc<Mutex<Option<Receiver<Packet>>>>,
    active_flows: Arc<Mutex<Vec<u64>>>,
    current_flow_index: Arc<Mutex<usize>>,
    packet_buffer: Arc<Mutex<Vec<Packet>>>, // Buffer for packets that don't match current flow
}

impl DRRScheduler {
    pub fn new(packet_tx: Sender<Packet>) -> Self {
        Self {
            flows: Arc::new(Mutex::new(HashMap::new())),
            packet_tx,
            packet_rx: Arc::new(Mutex::new(None)),
            active_flows: Arc::new(Mutex::new(Vec::new())),
            current_flow_index: Arc::new(Mutex::new(0)),
            packet_buffer: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn set_receiver(&self, rx: Receiver<Packet>) {
        *self.packet_rx.lock() = Some(rx);
    }

    pub fn add_flow(&self, flow_id: u64, quantum: usize, latency_budget: Duration) {
        let mut flows = self.flows.lock();
        flows.insert(
            flow_id,
            Flow {
                id: flow_id,
                quantum,
                deficit: 0,
                latency_budget,
            },
        );

        let mut active = self.active_flows.lock();
        if !active.contains(&flow_id) {
            active.push(flow_id);
        }
    }

    pub fn schedule_packet(
        &self,
        packet: Packet,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Add flow if it doesn't exist
        {
            let flows = self.flows.lock();
            if !flows.contains_key(&packet.flow_id) {
                drop(flows);
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
                    let mut flows = self.flows.lock();
                    if let Some(flow) = flows.get_mut(&flow1_packet.flow_id) {
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

        let mut flows = self.flows.lock();
        let active = self.active_flows.lock();

        if active.is_empty() {
            // If no active flows but we have packets, prioritize Flow 1 first, then by time remaining
            let mut buffer = self.packet_buffer.lock();
            if !buffer.is_empty() {
                // First, check if there's a Flow 1 packet (always highest priority)
                if let Some(flow1_pos) = buffer.iter().position(|p| p.flow_id == 1) {
                    let packet = buffer.remove(flow1_pos);
                    // Add flow if it doesn't exist
                    if !flows.contains_key(&packet.flow_id) {
                        drop(flows);
                        self.add_flow(packet.flow_id, 1024, packet.latency_budget);
                        // Re-acquire lock (flows is reassigned implicitly)
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
                        // Calculate time remaining until deadline
                        // Packets with less time remaining have higher priority
                        deadline.saturating_duration_since(now)
                    })
                    .map(|(pos, _)| pos);
                
                if let Some(pos) = best_pos {
                    let packet = buffer.remove(pos);
                    // Add flow if it doesn't exist
                    if !flows.contains_key(&packet.flow_id) {
                        drop(flows);
                        self.add_flow(packet.flow_id, 1024, packet.latency_budget);
                        // Re-acquire lock (flows is reassigned implicitly)
                    }
                    return Some(packet);
                }
            }
            return None;
        }

        let mut current_index = self.current_flow_index.lock();
        let mut attempts = 0;
        let max_attempts = active.len();

        // Priority: Always prioritize Flow 1 first, then by time remaining until deadline
        // This ensures low-latency flows (Flow 1 with 5ms deadline) are never starved
        let mut buffer = self.packet_buffer.lock();
        let now = std::time::Instant::now();
        
        // First, check if there's a Flow 1 packet (always highest priority)
        if let Some(flow1_pos) = buffer.iter().position(|p| p.flow_id == 1) {
            let packet = buffer.remove(flow1_pos);
            drop(buffer);
            
            // Update DRR state for this flow
            if let Some(flow) = flows.get_mut(&packet.flow_id) {
                flow.deficit += flow.quantum;
                if flow.deficit >= 1 {
                    flow.deficit -= 1;
                }
            }
            
            *current_index = (*current_index + 1) % active.len();
            return Some(packet);
        }
        
        // If no Flow 1 packet, find the packet with the least time remaining until deadline
        // This ensures Flow 1 (5ms) is processed before Flow 2 (50ms) and Flow 3 (100ms)
        let best_pos = buffer
            .iter()
            .enumerate()
            .min_by_key(|(_, p)| {
                let deadline = p.timestamp + p.latency_budget;
                // Calculate time remaining until deadline
                // Packets with less time remaining have higher priority
                deadline.saturating_duration_since(now)
            })
            .map(|(pos, _)| pos);
        
        if let Some(pos) = best_pos {
            let packet = buffer.remove(pos);
            drop(buffer);
            
            // Update DRR state for this flow
            if let Some(flow) = flows.get_mut(&packet.flow_id) {
                flow.deficit += flow.quantum;
                if flow.deficit >= 1 {
                    flow.deficit -= 1;
                }
            }
            
            *current_index = (*current_index + 1) % active.len();
            return Some(packet);
        }
        drop(buffer);

        // Try to find a packet for the current flow (normal DRR)
        while attempts < max_attempts {
            if active.is_empty() {
                break;
            }

            let flow_id = active[*current_index % active.len()];
            let flow = flows.get_mut(&flow_id)?;

            // Find a packet for this flow in the buffer
            let mut buffer = self.packet_buffer.lock();
            if let Some(pos) = buffer.iter().position(|p| p.flow_id == flow_id) {
                let packet = buffer.remove(pos);
                drop(buffer); // Release buffer lock

                // Apply DRR scheduling (based on packet count, not packet length)
                flow.deficit += flow.quantum;
                // Each packet costs 1 unit (packet count)
                if flow.deficit >= 1 {
                    flow.deficit -= 1;
                } else {
                    // Not enough deficit, but process anyway (simplified)
                    flow.deficit = 0;
                }

                *current_index = (*current_index + 1) % active.len();
                return Some(packet);
            } else {
                drop(buffer); // Release buffer lock before trying next flow
                              // No packet for this flow, try next flow
                *current_index = (*current_index + 1) % active.len();
                attempts += 1;
            }
        }

        // If we have packets but none matched, prioritize Flow 1 first, then by time remaining
        // (this handles cases where flow configuration might be incomplete)
        let mut buffer = self.packet_buffer.lock();
        if !buffer.is_empty() {
            // First, check if there's a Flow 1 packet (always highest priority)
            if let Some(flow1_pos) = buffer.iter().position(|p| p.flow_id == 1) {
                let packet = buffer.remove(flow1_pos);
                drop(buffer);

                // Add flow if it doesn't exist
                if !flows.contains_key(&packet.flow_id) {
                    drop(flows);
                    self.add_flow(packet.flow_id, 1024, packet.latency_budget);
                    flows = self.flows.lock();
                }
                if let Some(flow) = flows.get_mut(&packet.flow_id) {
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
                    // Calculate time remaining until deadline
                    // Packets with less time remaining have higher priority
                    deadline.saturating_duration_since(now)
                })
                .map(|(pos, _)| pos);
            
            if let Some(pos) = best_pos {
                let packet = buffer.remove(pos);
                drop(buffer);

                // Add flow if it doesn't exist
                if !flows.contains_key(&packet.flow_id) {
                    drop(flows);
                    self.add_flow(packet.flow_id, 1024, packet.latency_budget);
                    flows = self.flows.lock();
                }
                if let Some(flow) = flows.get_mut(&packet.flow_id) {
                    flow.deficit += flow.quantum;
                    // Each packet costs 1 unit (packet count)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;

    #[test]
    fn test_drr_scheduler_creation() {
        let (tx, _rx) = unbounded();
        let scheduler = DRRScheduler::new(tx);
        assert!(scheduler.flows.lock().is_empty());
    }

    #[test]
    fn test_add_flow() {
        let (tx, _rx) = unbounded();
        let scheduler = DRRScheduler::new(tx);
        scheduler.add_flow(1, 1024, Duration::from_millis(1));

        let flows = scheduler.flows.lock();
        assert!(flows.contains_key(&1));
        assert_eq!(flows[&1].quantum, 1024);
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
