use crate::drr_scheduler::Packet;
use crossbeam_channel::{unbounded, Receiver, Sender};
use parking_lot::Mutex;
use std::sync::Arc;

pub struct Queue {
    sender: Sender<Packet>,
    receiver: Arc<Mutex<Receiver<Packet>>>,
}

impl Queue {
    pub fn new() -> Self {
        let (tx, rx) = unbounded();
        Self {
            sender: tx,
            receiver: Arc::new(Mutex::new(rx)),
        }
    }

    pub fn sender(&self) -> Sender<Packet> {
        self.sender.clone()
    }

    pub fn receiver(&self) -> Arc<Mutex<Receiver<Packet>>> {
        self.receiver.clone()
    }

    pub fn send(&self, packet: Packet) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.sender.send(packet)?;
        Ok(())
    }

    pub fn try_recv(&self) -> Result<Packet, crossbeam_channel::TryRecvError> {
        self.receiver.lock().try_recv()
    }

    #[allow(dead_code)]
    pub fn recv(&self) -> Result<Packet, crossbeam_channel::RecvError> {
        self.receiver.lock().recv()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.receiver.lock().is_empty()
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.receiver.lock().len()
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
    use crate::drr_scheduler::Packet;
    use std::time::{Duration, Instant};

    #[test]
    fn test_queue_creation() {
        let queue = Queue::new();
        assert!(queue.is_empty());
    }

    #[test]
    fn test_queue_send_recv() {
        let queue = Queue::new();

        let packet = Packet {
            flow_id: 1,
            data: vec![1, 2, 3],
            timestamp: Instant::now(),
            latency_budget: Duration::from_millis(1),
        };

        queue.send(packet.clone()).unwrap();
        assert!(!queue.is_empty());

        let received = queue.recv().unwrap();
        assert_eq!(received.flow_id, packet.flow_id);
        assert_eq!(received.data, packet.data);
    }
}
