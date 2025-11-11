//! Packet representation shared by all scheduler stages.

use crate::buffer_pool::{lease, BufferHandle};
use crate::priority::Priority;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static PACKET_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Maximum payload size handled by the pipeline (standard Ethernet MTU).
pub const MAX_PACKET_SIZE: usize = 1500;

/// Lightweight representation of a work unit travelling through the pipeline.
///
/// Each [`Packet`] captures payload bytes, their latency budget, and the [`Priority`] used by the
/// schedulers. The timestamp is filled when the packet enters the pipeline so downstream stages can
/// compute latency and deadline misses.
#[derive(Debug, Clone)]
pub struct Packet {
    #[cfg_attr(not(test), allow(dead_code))]
    pub flow_id: u64,
    pub id: u64,
    buffer: BufferHandle,
    len: usize,
    pub timestamp: Instant,
    pub latency_budget: Duration,
    pub priority: Priority,
}

impl Packet {
    /// Create a packet while ensuring the flow/priorities are consistent.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(priority: Priority, payload: &[u8], latency_budget: Duration) -> Packet {
        let len = payload.len().min(MAX_PACKET_SIZE);
        let mut lease = lease(len);
        lease.as_mut_slice()[..len].copy_from_slice(&payload[..len]);
        let buffer = lease.freeze(len);
        Packet {
            flow_id: priority.flow_id(),
            id: PACKET_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
            priority,
            timestamp: Instant::now(),
            latency_budget,
            len,
            buffer,
        }
    }

    /// Instantiate a packet directly from a pooled buffer.
    pub fn from_buffer(
        priority: Priority,
        buffer: BufferHandle,
        latency_budget: Duration,
    ) -> Packet {
        let len = buffer.len().min(MAX_PACKET_SIZE);
        Packet {
            flow_id: priority.flow_id(),
            id: PACKET_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
            priority,
            timestamp: Instant::now(),
            latency_budget,
            len,
            buffer,
        }
    }

    /// Borrow the payload as a slice.
    pub fn payload(&self) -> &[u8] {
        let slice = self.buffer.as_slice();
        &slice[..self.len.min(slice.len())]
    }

    /// Current payload length.
    pub fn len(&self) -> usize {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn packet_builder_sets_priority() {
        let p = Packet::new(Priority::Medium, &[1, 2, 3], Duration::from_millis(10));
        assert_eq!(p.priority, Priority::Medium);
        assert_eq!(p.flow_id, Priority::Medium.flow_id());
        assert_eq!(p.payload(), &[1, 2, 3]);
    }
}
