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
///
/// Packets use zero-copy buffer handles from the buffer pool, allowing efficient cloning without
/// copying payload data. The buffer is automatically recycled when the last reference is dropped.
#[derive(Debug, Clone)]
pub struct Packet {
    /// Unique, monotonically increasing packet ID (assigned atomically on creation).
    ///
    /// Used by egress DRR to verify per-priority packet ordering. IDs are assigned using
    /// `AtomicU64::fetch_add`, ensuring uniqueness across all packets regardless of which
    /// thread creates them.
    pub id: u64,
    /// Shared handle to the pooled buffer containing the packet payload (zero-copy).
    ///
    /// The buffer is wrapped in `Arc` to enable cloning without copying data. When the last
    /// `Packet` referencing a buffer is dropped, the buffer is automatically recycled back
    /// to the pool via `BufferInner::drop`.
    buffer: BufferHandle,
    /// Actual payload length (may be less than buffer capacity).
    ///
    /// This is the number of valid bytes in the buffer, not the buffer's allocated size.
    /// Used to slice the buffer when accessing payload data.
    len: usize,
    /// Timestamp when the packet entered the pipeline (ingress DRR).
    ///
    /// Used by downstream stages to compute latency (time from ingress to egress) and
    /// detect deadline misses. Set to `Instant::now()` when the packet is created.
    pub timestamp: Instant,
    /// Maximum allowed latency for this packet (latency budget).
    ///
    /// Used by EDF schedulers to compute deadlines: `deadline = timestamp + latency_budget`.
    /// Packets that exceed this budget are marked as deadline misses in metrics.
    pub latency_budget: Duration,
    /// Priority class of this packet (High, Medium, Low, or BestEffort).
    ///
    /// Determines which scheduler queues the packet is routed to and which latency budget
    /// is applied. Used throughout the pipeline for scheduling decisions.
    pub priority: Priority,
}

impl Packet {
    /// Instantiate a packet directly from a pooled buffer.
    ///
    /// Creates a new packet with a unique ID, current timestamp, and the provided priority
    /// and latency budget. The buffer's length is clamped to `MAX_PACKET_SIZE` to prevent
    /// oversized packets.
    ///
    /// # Arguments
    /// * `priority` - Priority class for this packet
    /// * `buffer` - Shared buffer handle from the buffer pool (zero-copy)
    /// * `latency_budget` - Maximum allowed latency for this packet
    ///
    /// # Returns
    /// A new `Packet` with a unique ID and current timestamp
    ///
    /// # Thread Safety
    /// ID assignment uses `AtomicU64::fetch_add` with `Relaxed` ordering, ensuring unique
    /// IDs even when packets are created concurrently from multiple threads.
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

    /// Borrow the payload as a read-only slice.
    ///
    /// Returns a slice containing the valid payload bytes. The slice length is the minimum
    /// of the packet's declared length and the buffer's actual length, ensuring safe access
    /// even if the buffer was truncated.
    ///
    /// # Returns
    /// A read-only slice of the packet payload bytes
    ///
    /// # Performance
    /// O(1) operation. No copying occurs; the slice is a view into the shared buffer.
    pub fn payload(&self) -> &[u8] {
        let slice = self.buffer.as_slice();
        &slice[..self.len.min(slice.len())]
    }

    /// Get the current payload length in bytes.
    ///
    /// Returns the number of valid bytes in the packet, which may be less than the buffer's
    /// allocated capacity.
    ///
    /// # Returns
    /// Payload length in bytes (always ≤ `MAX_PACKET_SIZE`)
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
