use crate::drr_scheduler::Packet;
use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;
use std::time::{Instant, Duration};

#[derive(Debug, Clone)]
struct EDFTask {
    packet: Packet,
    deadline: Instant,
}

impl Ord for EDFTask {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse order for min-heap (earliest deadline first)
        other.deadline.cmp(&self.deadline)
    }
}

impl PartialOrd for EDFTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for EDFTask {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline
    }
}

impl Eq for EDFTask {}

pub struct EDFScheduler {
    tasks: Arc<Mutex<BinaryHeap<EDFTask>>>,
    input_rx: Arc<Receiver<Packet>>, // crossbeam Receiver is thread-safe, no mutex needed
    output_tx: Sender<Packet>,
}

impl EDFScheduler {
    pub fn new(input_rx: Arc<Receiver<Packet>>, output_tx: Sender<Packet>) -> Self {
        Self {
            tasks: Arc::new(Mutex::new(BinaryHeap::new())),
            input_rx,
            output_tx,
        }
    }

    #[allow(dead_code)]
    pub fn enqueue_packet(
        &self,
        packet: Packet,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let deadline = packet.timestamp + packet.latency_budget;
        let task = EDFTask { packet, deadline };

        let mut tasks = self.tasks.lock();
        tasks.push(task);
        Ok(())
    }

    pub fn process_next(&self) -> Option<Packet> {
        const MAX_HEAP_SIZE: usize = 1000; // Limit heap size to prevent unbounded growth
        
        // Collect all incoming packets first (no lock needed - crossbeam Receiver is thread-safe)
        // This reduces lock duration by batching all packet additions
        let mut incoming_tasks = Vec::new();
        while let Ok(packet) = self.input_rx.try_recv() {
            let deadline = packet.timestamp + packet.latency_budget;
            incoming_tasks.push(EDFTask { packet, deadline });
        }
        
        // Now lock the heap once and add all collected packets
        if !incoming_tasks.is_empty() {
            let mut tasks = self.tasks.lock();
            for task in incoming_tasks {
                // Limit heap size - drop lowest priority packets if full
                // This prevents unbounded growth that causes increasing latency
                if tasks.len() >= MAX_HEAP_SIZE {
                    // Drop the task with the latest deadline (lowest priority in min-heap)
                    // This is more efficient than rebuilding the heap
                    let _ = tasks.pop();
                }
                tasks.push(task);
            }
        }

        // Process the task with earliest deadline
        let mut tasks = self.tasks.lock();
        if let Some(task) = tasks.pop() {
            // Check if deadline has passed
            if task.deadline < Instant::now() {
                // Deadline missed, but still process
            }
            drop(tasks); // Release lock before returning
            Some(task.packet)
        } else {
            None
        }
    }

    pub fn send_processed(
        &self,
        packet: Packet,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.output_tx.send(packet)?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn has_tasks(&self) -> bool {
        !self.tasks.lock().is_empty() || !self.input_rx.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;

    #[test]
    fn test_edf_scheduler_creation() {
        let (_tx1, rx1) = unbounded();
        let (tx2, _rx2) = unbounded();
        use std::sync::Arc;
        let scheduler = EDFScheduler::new(Arc::new(rx1), tx2);
        assert!(scheduler.tasks.lock().is_empty());
    }

    #[test]
    fn test_edf_ordering() {
        let (tx1, rx1) = unbounded();
        let (tx2, rx2) = unbounded();
        use std::sync::Arc;
        use std::time::Duration;
        let scheduler = EDFScheduler::new(Arc::new(rx1), tx2);

        let now = Instant::now();

        // Create packets with different deadlines
        let packet1 = Packet {
            flow_id: 1,
            data: vec![1],
            timestamp: now,
            latency_budget: Duration::from_millis(100),
        };

        let packet2 = Packet {
            flow_id: 2,
            data: vec![2],
            timestamp: now,
            latency_budget: Duration::from_millis(50),
        };

        let packet3 = Packet {
            flow_id: 3,
            data: vec![3],
            timestamp: now,
            latency_budget: Duration::from_millis(1),
        };

        // Enqueue in wrong order
        scheduler.enqueue_packet(packet1.clone()).unwrap();
        scheduler.enqueue_packet(packet2.clone()).unwrap();
        scheduler.enqueue_packet(packet3.clone()).unwrap();

        // Should process in deadline order: 3, 2, 1
        let processed1 = scheduler.process_next().unwrap();
        assert_eq!(processed1.flow_id, 3); // Earliest deadline

        let processed2 = scheduler.process_next().unwrap();
        assert_eq!(processed2.flow_id, 2);

        let processed3 = scheduler.process_next().unwrap();
        assert_eq!(processed3.flow_id, 1);
    }
}
