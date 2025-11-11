//! Legacy bounded queue wrapper.
//!
//! The main pipeline uses per-priority crossbeam channels directly, but some tests and metrics
//! helpers still reference this minimal queue abstraction. It remains for backwards compatibility.

use crate::packet::Packet;
use crossbeam_channel::{bounded, Receiver, Sender};
use std::sync::Arc;

/// Thin wrapper around a bounded crossbeam channel for packets.
pub struct Queue {
    sender: Sender<Packet>,
    receiver: Arc<Receiver<Packet>>, // crossbeam Receiver is already thread-safe, no mutex needed
    capacity: usize,
}

#[allow(dead_code)] // Old Queue struct, kept for reference but not used in new architecture
impl Queue {
    pub fn new() -> Self {
        let capacity = 1;
        let (tx, rx) = bounded(capacity);
        Self {
            sender: tx,
            receiver: Arc::new(rx), // No mutex needed - crossbeam Receiver is thread-safe
            capacity,
        }
    }

    pub fn sender(&self) -> Sender<Packet> {
        self.sender.clone()
    }

    pub fn receiver(&self) -> Arc<Receiver<Packet>> {
        self.receiver.clone()
    }

    pub fn send(&self, packet: Packet) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.sender.send(packet)?;
        Ok(())
    }

    pub fn try_recv(&self) -> Result<Packet, crossbeam_channel::TryRecvError> {
        self.receiver.try_recv() // Direct call - no mutex needed
    }

    #[allow(dead_code)]
    pub fn recv(&self) -> Result<Packet, crossbeam_channel::RecvError> {
        self.receiver.recv() // Direct call - no mutex needed
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.receiver.is_empty() // Direct call - no mutex needed
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.receiver.len() // Direct call - no mutex needed
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn occupancy(&self) -> usize {
        self.receiver.len() // Direct call - no mutex needed
    }

    pub fn occupancy_percent(&self) -> f64 {
        let len = self.occupancy();
        if self.capacity > 0 {
            (len as f64 / self.capacity as f64) * 100.0
        } else {
            0.0
        }
    }
}

impl Default for Queue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::Packet;
    use crate::priority::Priority;
    use std::time::Duration;

    #[test]
    fn test_queue_creation() {
        let queue = Queue::new();
        assert!(queue.is_empty());
    }

    #[test]
    fn test_queue_send_recv() {
        let queue = Queue::new();

        let packet = Packet::new(Priority::High, &[1, 2, 3], Duration::from_millis(1));

        queue.send(packet.clone()).unwrap();
        assert!(!queue.is_empty());

        let received = queue.recv().unwrap();
        assert_eq!(received.priority, packet.priority);
        assert_eq!(received.payload(), packet.payload());
    }
}
