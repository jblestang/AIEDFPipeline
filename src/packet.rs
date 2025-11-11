//! Packet representation shared by all scheduler stages.

#[cfg(test)]
use crate::buffer_pool::lease;
use crate::buffer_pool::BufferHandle;
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
    pub id: u64,
    buffer: BufferHandle,
    len: usize,
    pub timestamp: Instant,
    pub latency_budget: Duration,
    pub priority: Priority,
}

impl Packet {
    /// Instantiate a packet directly from a pooled buffer.
    pub fn from_buffer(
        priority: Priority,
        buffer: BufferHandle,
        latency_budget: Duration,
    ) -> Packet {
        let len = buffer.len().min(MAX_PACKET_SIZE);
        Packet {
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
        let mut lease = lease(3);
        lease.as_mut_slice()[..3].copy_from_slice(&[1, 2, 3]);
        let buffer = lease.freeze(3);
        let p = Packet::from_buffer(Priority::Medium, buffer, Duration::from_millis(10));
        assert_eq!(p.priority, Priority::Medium);
        assert_eq!(p.payload(), &[1, 2, 3]);
    }
}
