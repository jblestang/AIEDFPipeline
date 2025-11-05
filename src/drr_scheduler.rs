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
        // First, collect new packets from receiver into internal buffer
        {
            let rx_guard = self.packet_rx.lock();
            if let Some(rx) = rx_guard.as_ref() {
                let mut buffer = self.packet_buffer.lock();
                // Collect all available packets
                while let Ok(packet) = rx.try_recv() {
                    buffer.push(packet);
                }
            }
        }

        let mut flows = self.flows.lock();
        let active = self.active_flows.lock();

        if active.is_empty() {
            // If no active flows but we have packets, process first packet anyway
            let mut buffer = self.packet_buffer.lock();
            if !buffer.is_empty() {
                let packet = buffer.remove(0);
                // Add flow if it doesn't exist
                if !flows.contains_key(&packet.flow_id) {
                    drop(flows);
                    self.add_flow(packet.flow_id, 1024, packet.latency_budget);
                    // Re-acquire lock (flows is reassigned implicitly)
                }
                return Some(packet);
            }
            return None;
        }

        let mut current_index = self.current_flow_index.lock();
        let mut attempts = 0;
        let max_attempts = active.len();

        // Try to find a packet for the current flow
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

        // If we have packets but none matched, process the first one anyway
        // (this handles cases where flow configuration might be incomplete)
        let mut buffer = self.packet_buffer.lock();
        if !buffer.is_empty() {
            let packet = buffer.remove(0);
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
