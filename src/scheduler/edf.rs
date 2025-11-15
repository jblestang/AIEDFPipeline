//! Earliest Deadline First (EDF) Scheduler
//!
//! The EDF stage receives packets from per-priority bounded channels, keeps at most one head-of-line
//! packet buffered for each class, and always executes the task with the earliest absolute deadline.
//! Packets are then forwarded to the egress DRR stage while maintaining drop counters for metrics.
//!
//! Algorithm:
//! 1. Maintain one buffered packet per priority (head-of-line)
//! 2. Compare deadlines of all buffered packets
//! 3. Process the packet with the earliest deadline
//! 4. Refill the buffer for the processed priority from its input queue
//! 5. Forward processed packet to egress DRR (or drop if queue full)

// Import packet representation used throughout the pipeline
use crate::packet::Packet;
// Import priority classes and helper table structure
use crate::priority::{Priority, PriorityTable};
// Import lock-free channels for inter-thread communication
use crossbeam_channel::{Receiver, Sender};
// Import mutex for protecting the pending buffer table
use parking_lot::Mutex;
// Import ordering trait for deadline comparisons
use std::cmp::Ordering;
// Import atomic types for lock-free drop counters
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
// Import Arc for shared ownership
use std::sync::Arc;
// Import time types for deadlines
use std::time::Instant;

/// Task scheduled in the EDF scheduler, ordered by deadline (earliest first).
///
/// Contains a packet and its absolute deadline (timestamp + latency_budget).
/// The scheduler compares deadlines across all priorities to select the earliest.
#[derive(Debug, Clone)]
struct EDFTask {
    /// The packet to process
    packet: Packet,
    /// Absolute deadline: timestamp + latency_budget (when the packet must be completed)
    deadline: Instant,
}

impl Ord for EDFTask {
    /// Compare by deadline in reverse order to create a min-heap.
    ///
    /// This allows finding the earliest deadline by comparing tasks.
    /// The reverse order (`other.deadline.cmp(&self.deadline)`) means:
    /// - Earlier deadlines are considered "greater"
    /// - This enables min-heap behavior when using standard max-heap structures
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse order for min-heap (earliest deadline first)
        // Earlier deadlines are "greater" in this ordering
        other.deadline.cmp(&self.deadline)
    }
}

impl PartialOrd for EDFTask {
    /// Delegates to `Ord::cmp` for total ordering.
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for EDFTask {
    /// Two tasks are equal if they have the same deadline.
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline
    }
}

impl Eq for EDFTask {}

/// Drop counters exposed by the EDF stage for metrics collection.
///
/// Tracks packets dropped due to full output queues (egress DRR channels).
#[derive(Debug, Clone)]
pub struct EDFDropCounters {
    /// Heap drops remain for compatibility; the current algorithm never fills a heap.
    /// Always 0 in the current implementation (we use a per-priority buffer, not a heap).
    pub heap: u64,
    /// Per priority drops recorded when the EDF → egress channel is full.
    /// Incremented atomically when `try_send` to egress DRR fails.
    pub output: PriorityTable<u64>,
}

/// EDF scheduler that keeps one buffered packet per priority and executes the earliest deadline.
///
/// Algorithm:
/// - Maintains one head-of-line packet per priority in a `pending` buffer
/// - On each `process_next()` call:
///   1. Refills empty buffers from input queues
///   2. Compares deadlines of all buffered packets
///   3. Selects the packet with the earliest deadline
///   4. Processes it (simulated CPU work)
///   5. Forwards it to egress DRR (or drops if queue full)
///
/// This design ensures fairness across priorities while always prioritizing urgent deadlines.
pub struct EDFScheduler {
    /// Per-priority input channels from ingress DRR scheduler
    input_queues: PriorityTable<Arc<Receiver<Packet>>>,
    /// Per-priority output channels to egress DRR scheduler
    output_queues: PriorityTable<Sender<Packet>>,
    /// Per-priority atomic drop counters (incremented when egress queue is full)
    output_drop_counters: PriorityTable<Arc<AtomicU64>>,
    /// Pending head-of-line packet for each priority.
    /// Protected by a mutex to allow concurrent access from multiple threads if needed.
    /// Each priority has at most one buffered packet (None if queue is empty).
    pending: Arc<Mutex<PriorityTable<Option<EDFTask>>>>,
    /// Retained for configuration compatibility; no heap is used in the current implementation.
    /// The scheduler uses a per-priority buffer instead of a single heap.
    _max_heap_size: usize,
}

impl EDFScheduler {
    /// Construct a new EDF scheduler from per-priority channels and a heap size hint.
    ///
    /// # Arguments
    /// * `input_queues` - Per-priority input channels from ingress DRR
    /// * `output_queues` - Per-priority output channels to egress DRR
    /// * `max_heap_size` - Unused (retained for compatibility, scheduler uses per-priority buffers)
    pub fn new(
        input_queues: PriorityTable<Arc<Receiver<Packet>>>, // Input channels per priority
        output_queues: PriorityTable<Sender<Packet>>,       // Output channels per priority
        max_heap_size: usize, // Unused: retained for API compatibility
    ) -> Self {
        // Initialize drop counters to zero for each priority
        let output_drop_counters = PriorityTable::from_fn(|_| Arc::new(AtomicU64::new(0)));
        Self {
            input_queues,         // Store input channels
            output_queues,        // Store output channels
            output_drop_counters, // Store atomic drop counters
            // Initialize pending buffer: one slot per priority, all empty (None)
            pending: Arc::new(Mutex::new(PriorityTable::from_fn(|_| None))),
            _max_heap_size: max_heap_size, // Store (unused) heap size hint
        }
    }

    /// Return the currently recorded drop counts (used by metrics).
    ///
    /// Reads the atomic drop counters for each priority and returns them in a snapshot.
    /// This is called periodically by the metrics collector to track packet drops.
    pub fn get_drop_counts(&self) -> EDFDropCounters {
        EDFDropCounters {
            heap: 0, // Always 0 (no heap in current implementation)
            // Read each drop counter atomically (relaxed ordering is sufficient for metrics)
            output: PriorityTable::from_fn(|priority| {
                self.output_drop_counters[priority].load(AtomicOrdering::Relaxed)
            }),
        }
    }

    /// Process the next available packet (if any) and return it for testing convenience.
    ///
    /// Algorithm:
    /// 1. Refill empty buffers from input queues (one packet per priority max)
    /// 2. Find the earliest deadline among all buffered packets
    /// 3. Process that packet (simulated CPU work)
    /// 4. Forward to egress DRR (or drop if queue full)
    ///
    /// # Returns
    /// `None` (return value is for testing convenience, not used in production)
    pub fn process_next(&self) -> Option<Packet> {
        // Acquire lock on pending buffer table
        let mut buffers = self.pending.lock();

        // ========================================================================
        // STEP 1: Fill empty buffers with head packets from input queues
        // ========================================================================
        // Fill empty buffers with the head packet from each input queue.
        // We maintain at most one packet per priority in the buffer (head-of-line).
        for priority in Priority::ALL {
            // Check if this priority's buffer is empty
            if buffers[priority].is_none() {
                // Try to receive a packet from the input queue (non-blocking)
                if let Ok(packet) = self.input_queues[priority].try_recv() {
                    // Calculate absolute deadline: ingress timestamp + latency budget
                    let deadline = packet.timestamp + packet.latency_budget;
                    // Store the packet in the buffer for this priority
                    buffers[priority] = Some(EDFTask { packet, deadline });
                }
                // If try_recv fails (queue empty), buffer remains None
            }
        }

        // ========================================================================
        // STEP 2: Find the earliest deadline among all buffered packets
        // ========================================================================
        // Determine the earliest deadline among the buffered packets.
        // We iterate through all priorities and compare deadlines to find the minimum.
        let mut earliest: Option<(Priority, Instant)> = None; // (priority, deadline) of earliest packet
        for priority in Priority::ALL {
            // Check if this priority has a buffered packet
            if let Some(task) = buffers[priority].as_ref() {
                match earliest {
                    // No earliest found yet: this is the first packet we've seen
                    None => earliest = Some((priority, task.deadline)),
                    // We have a candidate: compare deadlines
                    Some((_, current_deadline)) if task.deadline < current_deadline => {
                        // This packet has an earlier deadline: it becomes the new candidate
                        earliest = Some((priority, task.deadline));
                    }
                    // This packet has a later deadline: keep the current candidate
                    _ => {}
                }
            }
        }

        // Check if we found any packet to process
        let Some((selected_priority, _)) = earliest else {
            // No packets available: all buffers are empty
            return None;
        };

        // ========================================================================
        // STEP 3: Remove the selected packet from the buffer
        // ========================================================================
        // Take the selected packet out of the buffer (will be refilled on next call)
        let task = buffers[selected_priority]
            .take() // Remove from buffer (sets slot to None)
            .expect("pending task must exist for selected priority"); // Should never fail
                                                                      // Release the lock early (we no longer need the buffer)
        drop(buffers);

        // Extract the packet from the task
        let packet = task.packet;

        // ========================================================================
        // STEP 4: Simulate packet processing (busy-wait)
        // ========================================================================
        // Deterministic processing time based on payload size.
        // Base: 0.10 ms for small packets (<= 200 bytes)
        // Extra: linear scaling from 0 to 0.1 ms for packets between 200 and 1500 bytes
        // Maximum: ~0.20 ms for MTU-sized packets (1500 bytes)

        let base_ms = 0.10; // Base processing time (0.10 ms)
                            // Calculate extra processing time for larger packets
        let extra_ms = if packet.len() > 200 {
            // Clamp packet size to MTU (1500 bytes) to avoid unrealistic values
            let clamped = packet.len().min(1500);
            // Linear interpolation: 0 ms extra at 200 bytes, 0.1 ms extra at 1500 bytes
            // Formula: (size - 200) / (1500 - 200) * 0.1
            0.1 * ((clamped - 200) as f64 / 1300.0)
        } else {
            // No extra time for packets <= 200 bytes
            0.0
        };

        // Total processing time in milliseconds
        let processing_time_ms = base_ms + extra_ms;
        // Convert to Duration (divide by 1000 to get seconds)
        let processing_time = std::time::Duration::from_secs_f64(processing_time_ms / 1000.0);

        // Busy-wait loop: spin until processing time has elapsed
        let start = Instant::now();
        while start.elapsed() < processing_time {
            std::hint::spin_loop(); // CPU hint: indicates tight spin loop
        }

        // ========================================================================
        // STEP 5: Forward packet to egress DRR (or drop if queue full)
        // ========================================================================
        // Get the priority of the processed packet
        let priority = packet.priority;
        // Try to send to the egress DRR queue (non-blocking)
        if self.output_queues[priority].try_send(packet).is_err() {
            // Send failed (queue full): increment drop counter atomically
            // Relaxed ordering is sufficient: this is just a metric, not synchronization
            self.output_drop_counters[priority].fetch_add(1, AtomicOrdering::Relaxed);
        }

        // Return None (return value is for testing convenience)
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;
    use std::time::{Duration, Instant};

    struct SchedulerHarness {
        scheduler: EDFScheduler,
        inputs: PriorityTable<crossbeam_channel::Sender<Packet>>,
        outputs: PriorityTable<crossbeam_channel::Receiver<Packet>>,
    }

    impl SchedulerHarness {
        fn new(capacity: usize, max_heap: usize) -> Self {
            let mut input_senders = Vec::new();
            let mut input_receivers = Vec::new();
            for _ in Priority::ALL {
                let (tx, rx) = bounded(capacity);
                input_senders.push(tx);
                input_receivers.push(Arc::new(rx));
            }

            let mut output_senders = Vec::new();
            let mut output_receivers = Vec::new();
            for _ in Priority::ALL {
                let (tx, rx) = bounded(capacity);
                output_senders.push(tx);
                output_receivers.push(rx);
            }

            let scheduler = EDFScheduler::new(
                PriorityTable::from_vec(input_receivers),
                PriorityTable::from_vec(output_senders),
                max_heap,
            );

            Self {
                scheduler,
                inputs: PriorityTable::from_vec(input_senders),
                outputs: PriorityTable::from_vec(output_receivers),
            }
        }

        fn enqueue(
            &self,
            priority: Priority,
            data: Vec<u8>,
            latency_budget: Duration,
            timestamp: Instant,
        ) {
            let len = data.len();
            let mut lease = crate::buffer_pool::lease(len);
            lease.as_mut_slice()[..len].copy_from_slice(&data);
            let mut packet = Packet::from_buffer(priority, lease.freeze(len), latency_budget);
            packet.timestamp = timestamp;
            self.inputs[priority].send(packet).unwrap();
        }

        fn step(&self) {
            let _ = self.scheduler.process_next();
        }

        fn outputs(&self) -> &PriorityTable<crossbeam_channel::Receiver<Packet>> {
            &self.outputs
        }

        fn scheduler(&self) -> &EDFScheduler {
            &self.scheduler
        }
    }

    #[test]
    fn scheduler_initializes_empty() {
        let harness = SchedulerHarness::new(4, 16);
        let pending = harness.scheduler.pending.lock();
        assert!(Priority::ALL
            .iter()
            .all(|priority| pending[*priority].is_none()));
    }

    #[test]
    fn scheduler_processes_single_packet() {
        let harness = SchedulerHarness::new(4, 16);
        let now = Instant::now();
        harness.enqueue(Priority::High, vec![1], Duration::from_millis(10), now);
        harness.step();

        let mut observed = Vec::new();
        for priority in Priority::ALL {
            while let Ok(packet) = harness.outputs()[priority].try_recv() {
                observed.push(packet.priority);
            }
        }
        assert_eq!(observed, vec![Priority::High]);
    }

    #[test]
    fn scheduler_picks_earliest_deadline() {
        let harness = SchedulerHarness::new(4, 16);
        let now = Instant::now();

        harness.enqueue(Priority::High, vec![1], Duration::from_millis(100), now);
        harness.enqueue(Priority::Medium, vec![2], Duration::from_millis(5), now);

        harness.step();

        let medium_packet = harness.outputs()[Priority::Medium]
            .try_recv()
            .expect("expected medium priority packet first");
        assert_eq!(medium_packet.priority, Priority::Medium);

        harness.step();
        let high_packet = harness.outputs()[Priority::High]
            .try_recv()
            .expect("expected high priority packet afterwards");
        assert_eq!(high_packet.priority, Priority::High);
    }

    #[test]
    fn scheduler_counts_drops_when_output_full() {
        let harness = SchedulerHarness::new(1, 16);
        let now = Instant::now();

        harness.enqueue(Priority::High, vec![1], Duration::from_millis(5), now);
        harness.step();

        // Leave the first packet in the output queue so it remains full.
        harness.enqueue(Priority::High, vec![2], Duration::from_millis(5), now);
        harness.step();

        let drop_counts = harness.scheduler().get_drop_counts();
        assert_eq!(drop_counts.output[Priority::High], 1);
    }
}
