//! Multi-Worker Earliest Deadline First (EDF) Scheduler with Adaptive Load Balancing
//!
//! This module implements a parallel EDF scheduler that distributes work across multiple worker threads,
//! each handling a subset of priority classes. The scheduler uses:
//! - Per-worker EDF heaps to select the earliest-deadline packet
//! - Sequence tracking to maintain FIFO order per priority despite parallel execution
//! - Adaptive quotas that adjust based on observed processing times and latency budgets
//! - Guard windows to minimize latency for high-priority flows

// Import packet representation used throughout the pipeline
use crate::packet::Packet;
// Import priority classes and helper table structure
use crate::priority::{Priority, PriorityTable};
// Import drop counter structure for metrics compatibility
use crate::scheduler::edf::EDFDropCounters;
// Import lock-free channels for inter-thread communication
use crossbeam_channel::{self, Receiver, Sender, TryRecvError};
// Import ordering trait for deadline comparisons
use std::cmp::Ordering;
// Import BTreeMap for ordered sequence tracking, BinaryHeap for EDF scheduling
use std::collections::{BTreeMap, BinaryHeap};
// Import atomic types for lock-free counters and flags
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering as AtomicOrdering};
// Import Arc for shared ownership, Mutex for sequence tracker synchronization
use std::sync::{Arc, Mutex};
// Import thread utilities for spawning worker threads
use std::thread;
// Import time types for deadlines and latency budgets
use std::time::{Duration, Instant};

/// Worker priority assignments: defines which priorities each worker can process.
///
/// Worker 0: strictly HIGH priority only (dedicated high-latency core)
/// Worker 1: HIGH + MEDIUM priorities (can handle both high and medium latency flows)
/// Worker 2: HIGH + MEDIUM + LOW + BEST_EFFORT (handles all priorities, including best-effort)
///
/// This hierarchical assignment ensures:
/// - High-priority packets always have dedicated capacity (Worker 0)
/// - Medium-priority packets can use Workers 1 and 2 (2 workers max)
/// - Low-priority packets can only use Worker 2 (1 worker max)
/// - Best-effort packets can only use Worker 2 (1 worker max)
///
/// The assignment prevents priority inversion by ensuring higher priorities have more workers available.
const WORKER_ASSIGNMENTS: [&[Priority]; 3] = [
    &[Priority::High], // Worker 0: exclusive HIGH priority
    &[Priority::High, Priority::Medium], // Worker 1: HIGH and MEDIUM
    &[
        Priority::High,
        Priority::Medium,
        Priority::Low,
        Priority::BestEffort,
    ], // Worker 2: all priorities
];

/// Work item passed between dispatcher and workers.
///
/// Contains the packet to process along with its priority and a sequence number
/// that ensures per-priority FIFO ordering even when multiple workers process
/// packets in parallel.
#[derive(Clone)]
struct WorkItem {
    /// Priority class of the packet (High, Medium, Low, BestEffort)
    priority: Priority,
    /// Monotonically increasing sequence number assigned per priority
    /// Used by SequenceTracker to reorder out-of-order completions
    sequence: u64,
    /// The actual packet data to process
    packet: Packet,
}

/// Internal state of the sequence tracker, protected by a Mutex.
///
/// Maintains the next expected sequence number and a map of out-of-order packets
/// waiting to be emitted in the correct sequence.
struct SequenceInner {
    /// Next sequence number expected to be emitted (packets with this sequence can be sent immediately)
    next_emit: u64,
    /// Map of sequence number -> packet for packets that arrived out of order
    /// These packets are buffered until their sequence number matches next_emit
    pending: BTreeMap<u64, Packet>,
}

/// Per-priority sequence tracker ensuring FIFO ordering despite parallel worker execution.
///
/// When multiple workers process packets of the same priority in parallel, they may
/// complete in a different order than they were dispatched. This tracker:
/// 1. Assigns a monotonically increasing sequence number to each packet
/// 2. Buffers out-of-order completions
/// 3. Emits packets in sequence order, ensuring per-priority FIFO semantics
///
/// The sequence number is atomic (lock-free assignment), but completion requires
/// a mutex to coordinate reordering (low contention since it's per-priority).
struct SequenceTracker {
    /// Atomic counter for assigning sequence numbers (lock-free, per-priority)
    next_sequence: AtomicU64,
    /// Mutex-protected inner state for reordering completions
    inner: Mutex<SequenceInner>,
}

impl SequenceTracker {
    /// Create a new sequence tracker starting at sequence 0.
    ///
    /// Initializes the atomic sequence counter to 0 and creates an empty pending map.
    fn new() -> Self {
        Self {
            // Start sequence numbering at 0 (first packet gets sequence 0)
            next_sequence: AtomicU64::new(0),
            // Initialize inner state: expect sequence 0 next, no pending packets
            inner: Mutex::new(SequenceInner {
                next_emit: 0, // First packet to emit should have sequence 0
                pending: BTreeMap::new(), // No out-of-order packets yet
            }),
        }
    }

    /// Assign the next sequence number atomically (lock-free).
    ///
    /// This is called when a packet is dispatched to a worker. The sequence number
    /// is monotonically increasing per priority, ensuring we can detect out-of-order
    /// completions.
    ///
    /// Returns the assigned sequence number (previous value + 1).
    fn assign_sequence(&self) -> u64 {
        // Atomically increment and return the previous value (fetch-and-add)
        // Relaxed ordering is sufficient: we only need atomicity, not synchronization
        self.next_sequence.fetch_add(1, AtomicOrdering::Relaxed)
    }

    /// Complete a packet with the given sequence number, emitting it in order.
    ///
    /// If the sequence matches `next_emit`, the packet is sent immediately and we
    /// check for any consecutive pending packets that can now be emitted.
    /// Otherwise, the packet is buffered in the pending map until its sequence arrives.
    ///
    /// # Arguments
    /// * `sequence` - The sequence number assigned to this packet
    /// * `packet` - The completed packet to emit
    /// * `sender` - Channel to send the packet to (egress DRR)
    /// * `drop_counter` - Atomic counter incremented if the send fails (queue full)
    fn complete(
        &self,
        sequence: u64, // Sequence number of the completed packet
        packet: Packet, // The packet that was processed
        sender: &Sender<Packet>, // Output channel to egress DRR
        drop_counter: &AtomicU64, // Counter for dropped packets (queue full)
    ) {
        // Acquire the mutex to access inner state (needed for reordering logic)
        let mut inner = self.inner.lock().expect("sequence tracker poisoned");
        // Check if this packet is the next one expected in sequence
        if sequence == inner.next_emit {
            // Yes: send immediately (in-order completion)
            send_packet(sender, packet, drop_counter);
            // Update next_emit to the following sequence number
            let mut next_emit = inner.next_emit + 1;
            // Check if any consecutive packets are now ready (cascading emission)
            // This handles cases where multiple out-of-order packets arrive in quick succession
            while let Some(next_packet) = inner.pending.remove(&next_emit) {
                // Found a consecutive packet: emit it and continue checking
                send_packet(sender, next_packet, drop_counter);
                next_emit += 1; // Move to the next expected sequence
            }
            // Update the tracker's next_emit to reflect all emitted packets
            inner.next_emit = next_emit;
        } else {
            // No: packet arrived out of order, buffer it until its sequence arrives
            inner.pending.insert(sequence, packet);
        }
    }
}

/// Attempt to send a packet to the output channel, incrementing drop counter on failure.
///
/// Uses `try_send` to avoid blocking if the egress queue is full. If the send fails,
/// the packet is dropped and the drop counter is incremented atomically.
///
/// # Arguments
/// * `sender` - The output channel sender (non-blocking)
/// * `packet` - The packet to send
/// * `drop_counter` - Atomic counter incremented on send failure
fn send_packet(sender: &Sender<Packet>, packet: Packet, drop_counter: &AtomicU64) {
    // Try to send without blocking (returns error if queue is full)
    if sender.try_send(packet).is_err() {
        // Send failed (queue full): increment drop counter atomically
        // Relaxed ordering is sufficient: this is just a metric, not synchronization
        drop_counter.fetch_add(1, AtomicOrdering::Relaxed);
    }
}

/// Router that coordinates packet completion and sequence tracking per priority.
///
/// Maintains separate sequence trackers for each priority class, ensuring that
/// packets of the same priority are emitted in FIFO order even when processed
/// by different workers in parallel.
struct CompletionRouter {
    /// Per-priority sequence trackers (one per priority class)
    trackers: PriorityTable<Arc<SequenceTracker>>,
    /// Per-priority output channels to egress DRR scheduler
    output_queues: PriorityTable<Sender<Packet>>,
    /// Per-priority drop counters (incremented when egress queue is full)
    drop_counters: PriorityTable<Arc<AtomicU64>>,
}

impl CompletionRouter {
    /// Create a new completion router with per-priority trackers and output queues.
    ///
    /// # Arguments
    /// * `output_queues` - Per-priority channels to the egress DRR scheduler
    fn new(output_queues: PriorityTable<Sender<Packet>>) -> Self {
        Self {
            // Create a sequence tracker for each priority (independent tracking per priority)
            trackers: PriorityTable::from_fn(|_| Arc::new(SequenceTracker::new())),
            // Store the output channels (one per priority)
            output_queues,
            // Initialize drop counters to zero for each priority
            drop_counters: PriorityTable::from_fn(|_| Arc::new(AtomicU64::new(0))),
        }
    }

    /// Complete a work item, routing it through the appropriate sequence tracker.
    ///
    /// This is called by workers after processing a packet. The sequence tracker
    /// ensures the packet is emitted in order for its priority class.
    ///
    /// # Arguments
    /// * `work_item` - The completed work item (contains priority, sequence, packet)
    fn complete(&self, work_item: WorkItem) {
        // Get the sequence tracker for this packet's priority
        let tracker = &self.trackers[work_item.priority];
        // Get the output channel for this priority
        let sender = &self.output_queues[work_item.priority];
        // Get the drop counter for this priority
        let drop_counter = &self.drop_counters[work_item.priority];
        // Route the packet through the sequence tracker (handles reordering)
        tracker.complete(work_item.sequence, work_item.packet, sender, drop_counter);
    }

    /// Get the current drop counts for all priorities.
    ///
    /// Returns a table of drop counts (packets dropped due to full egress queues).
    fn drop_counts(&self) -> PriorityTable<u64> {
        // Read each drop counter atomically (relaxed ordering is sufficient for metrics)
        PriorityTable::from_fn(|priority| {
            self.drop_counters[priority].load(AtomicOrdering::Relaxed)
        })
    }

    /// Prepare a work item by assigning a sequence number.
    ///
    /// Called when a packet is dispatched to a worker. Assigns a monotonically
    /// increasing sequence number for the packet's priority class.
    ///
    /// # Arguments
    /// * `priority` - The priority class of the packet
    /// * `packet` - The packet to process
    /// * `_deadline` - Unused (kept for API compatibility)
    ///
    /// # Returns
    /// A WorkItem with the assigned sequence number
    fn prepare(&self, priority: Priority, packet: Packet, _deadline: Instant) -> WorkItem {
        // Get the sequence tracker for this priority
        let tracker = &self.trackers[priority];
        // Assign the next sequence number atomically (lock-free)
        let sequence = tracker.assign_sequence();
        // Create and return the work item
        WorkItem {
            priority, // Preserve the priority
            sequence, // Include the assigned sequence number
            packet, // Include the packet data
        }
    }
}

/// Calculate the simulated processing duration for a packet based on its size.
///
/// Processing time is size-dependent:
/// - Base: 0.05 ms for packets <= 200 bytes
/// - Extra: linear scaling from 0 to 0.1 ms for packets between 200 and 1500 bytes
/// - Maximum: ~0.15 ms for MTU-sized packets (1500 bytes)
///
/// This simulates real CPU work that scales with packet size (e.g., parsing, checksumming).
///
/// # Arguments
/// * `packet` - The packet to calculate processing time for
///
/// # Returns
/// The duration to simulate processing this packet
pub(crate) fn processing_duration(packet: &Packet) -> Duration {
    // Base processing time: 0.05 ms for small packets (<= 200 bytes)
    let base_ms = 0.05;
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
    // Convert milliseconds to Duration (divide by 1000 to get seconds)
    Duration::from_secs_f64((base_ms + extra_ms) / 1000.0)
}

/// Attempt to push an item into the heap while respecting capacity limits.
///
/// Enforces both per-priority quotas and a global per-worker backlog limit to prevent
/// runaway heap growth and ensure fair resource allocation across priorities.
///
/// # Arguments
/// * `heap` - The EDF heap to push into
/// * `counts` - Per-priority count of items currently in the heap
/// * `total_count` - Total count of items in the heap
/// * `per_priority_limit` - Maximum items allowed per priority for this worker
/// * `total_limit` - Maximum total items allowed in this worker's heap
/// * `item` - The scheduled item to push
///
/// # Returns
/// `true` if the item was pushed, `false` if capacity limits prevented it
fn push_with_capacity(
    heap: &mut BinaryHeap<ScheduledItem>, // The EDF min-deadline heap
    counts: &mut PriorityTable<usize>, // Per-priority counts (tracked locally)
    total_count: &mut usize, // Total items in heap (tracked locally)
    per_priority_limit: &PriorityTable<usize>, // Per-priority quotas (from adaptive controller)
    total_limit: usize, // Global worker backlog limit (from adaptive controller)
    item: ScheduledItem, // The item to push (contains deadline and work item)
) -> bool {
    // Extract the priority from the work item
    let priority = item.work_item.priority;
    // Get the per-priority limit for this worker and priority
    let limit_for_priority = per_priority_limit[priority];
    // Reject immediately if this worker is not allowed to process this priority
    // (limit == 0 means the worker doesn't handle this priority)
    if limit_for_priority == 0 {
        return false; // Worker cannot handle this priority
    }
    // Check if we've exceeded either the global limit or the per-priority quota
    // We also enforce the global per-worker backlog ceiling to avoid runaway heaps.
    if *total_count >= total_limit || counts[priority] >= limit_for_priority {
        return false; // Capacity limits reached, cannot admit more packets
    }

    // All checks passed: push the item into the heap
    heap.push(item);

    // Update counters to reflect the new item
    counts[priority] += 1; // Increment per-priority count
    *total_count += 1; // Increment total count
    true // Successfully pushed
}

/// Item scheduled in the EDF heap, ordered by deadline (earliest first).
///
/// The heap is a min-heap on deadline: the item with the earliest deadline
/// is always at the top. This is achieved by reversing the comparison in `Ord`.
#[derive(Clone)]
struct ScheduledItem {
    /// Absolute deadline: timestamp + latency_budget (when the packet must be completed)
    deadline: Instant,
    /// The work item containing the packet, priority, and sequence number
    work_item: WorkItem,
}

impl PartialEq for ScheduledItem {
    /// Two items are equal if they have the same deadline.
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline
    }
}

impl Eq for ScheduledItem {}

impl PartialOrd for ScheduledItem {
    /// Delegates to `Ord::cmp` for total ordering.
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScheduledItem {
    /// Compare by deadline in reverse order to create a min-heap.
    ///
    /// BinaryHeap is a max-heap by default, so we reverse the comparison:
    /// - `other.deadline.cmp(&self.deadline)` means earlier deadlines are "greater"
    /// - This makes the earliest-deadline item rise to the top of the heap
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse order: earlier deadlines are "greater" (for min-heap behavior)
        other.deadline.cmp(&self.deadline)
    }
}

struct AdaptiveController {
    worker_total_limits: Vec<AtomicUsize>,
    worker_priority_limits: Vec<PriorityTable<AtomicUsize>>,
    guard_slice_us: AtomicU64,
    guard_threshold_us: PriorityTable<AtomicU64>,
    processing_ema_ns: PriorityTable<AtomicU64>,
}

impl AdaptiveController {
    fn new() -> Self {
        let worker_total_limits = vec![
            AtomicUsize::new(16),
            AtomicUsize::new(64),
            AtomicUsize::new(256),
        ];
        let worker_priority_limits = vec![
            PriorityTable::from_fn(|priority| {
                let initial = if priority == Priority::High { 16 } else { 0 };
                AtomicUsize::new(initial)
            }),
            PriorityTable::from_fn(|priority| {
                let initial = match priority {
                    Priority::High => 4,
                    Priority::Medium => 60,
                    _ => 0,
                };
                AtomicUsize::new(initial)
            }),
            PriorityTable::from_fn(|priority| {
                let initial = match priority {
                    Priority::High => 4,
                    Priority::Medium => 60,
                    Priority::Low => 64,
                    Priority::BestEffort => 128,
                };
                AtomicUsize::new(initial)
            }),
        ];
        let guard_threshold_us = PriorityTable::from_fn(|priority| match priority {
            Priority::Medium => AtomicU64::new(80),
            Priority::Low => AtomicU64::new(80),
            _ => AtomicU64::new(0),
        });
        let processing_ema_ns = PriorityTable::from_fn(|priority| {
            let ns = match priority {
                Priority::High => 100_000,
                Priority::Medium => 120_000,
                Priority::Low => 200_000,
                Priority::BestEffort => 200_000,
            };
            AtomicU64::new(ns)
        });
        Self {
            worker_total_limits,
            worker_priority_limits,
            guard_slice_us: AtomicU64::new(10),
            guard_threshold_us,
            processing_ema_ns,
        }
    }

    fn total_limit(&self, worker_id: usize) -> usize {
        self.worker_total_limits[worker_id]
            .load(AtomicOrdering::Relaxed)
            .max(1)
    }

    fn set_total_limit(&self, worker_id: usize, value: usize) {
        self.worker_total_limits[worker_id].store(value.max(1), AtomicOrdering::Relaxed);
    }

    fn priority_limits(&self, worker_id: usize) -> PriorityTable<usize> {
        PriorityTable::from_fn(|priority| {
            self.worker_priority_limits[worker_id][priority].load(AtomicOrdering::Relaxed)
        })
    }

    fn set_priority_limit(&self, worker_id: usize, priority: Priority, value: usize) {
        self.worker_priority_limits[worker_id][priority].store(value, AtomicOrdering::Relaxed);
    }

    fn guard_slice(&self) -> Duration {
        let micros = self.guard_slice_us.load(AtomicOrdering::Relaxed).max(1);
        Duration::from_micros(micros)
    }

    fn set_guard_slice_us(&self, micros: u64) {
        self.guard_slice_us
            .store(micros.max(1), AtomicOrdering::Relaxed);
    }

    fn guard_threshold(&self, priority: Priority, latency_budget: Duration) -> Option<Duration> {
        match priority {
            Priority::High | Priority::BestEffort => None,
            _ => {
                let micros = self.guard_threshold_us[priority].load(AtomicOrdering::Relaxed);
                if micros == 0 {
                    None
                } else {
                    let threshold = Duration::from_micros(micros);
                    Some(threshold.min(latency_budget))
                }
            }
        }
    }

    fn set_guard_threshold_us(&self, priority: Priority, micros: u64) {
        self.guard_threshold_us[priority].store(micros, AtomicOrdering::Relaxed);
    }

    /// Update the EMA of the processing duration for `priority`.
    fn observe_processing_time(&self, priority: Priority, duration: Duration) {
        let new_ns = duration.as_nanos() as u64;
        let slot = &self.processing_ema_ns[priority];
        let _ = slot.fetch_update(AtomicOrdering::Relaxed, AtomicOrdering::Relaxed, |prev| {
            let prev = prev.max(1);
            let updated = (prev * 7 + new_ns) / 8;
            Some(updated)
        });
    }

    fn average_processing_ns(&self, priority: Priority) -> u64 {
        self.processing_ema_ns[priority]
            .load(AtomicOrdering::Relaxed)
            .max(1)
    }

    /// Background loop that reshapes worker quotas based on recent processing times.
    ///
    /// Every 100 ms we:
    /// 1. Estimate the safe backlog for each priority (`target ≈ 0.9 × budget / avg_duration`);
    /// 2. Distribute the capacity across workers according to their assignments;
    /// 3. Adjust guard thresholds and slice duration to reflect the new budget share.
    fn autobalance_loop(
        self: Arc<Self>,
        running: Arc<AtomicBool>,
        expected_latencies: Arc<PriorityTable<Duration>>,
        worker_priority_counters: Vec<PriorityTable<Arc<AtomicUsize>>>,
    ) {
        const HIGH_PRIMARY_MAX: usize = 64;
        const HIGH_SPILL_MAX: usize = 16;
        const MEDIUM_W1_MAX: usize = 160;
        const MEDIUM_W2_MAX: usize = 160;
        const LOW_MAX: usize = 192;
        const BEST_EFFORT_MAX: usize = 128;

        while running.load(AtomicOrdering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));

            let mut new_totals = vec![0usize; WORKER_ASSIGNMENTS.len()];
            let mut new_limits: Vec<PriorityTable<usize>> = WORKER_ASSIGNMENTS
                .iter()
                .map(|_| PriorityTable::from_fn(|_| 0usize))
                .collect();

            let mut guard_slice_candidates = Vec::new();

            for priority in Priority::ALL {
                if matches!(priority, Priority::BestEffort) {
                    continue;
                }
                let avg_ns = self.average_processing_ns(priority);
                let budget_ns = expected_latencies[priority].as_nanos() as u64;
                if budget_ns == 0 || avg_ns == 0 {
                    continue;
                }
                let capacity = ((budget_ns as f64 / avg_ns as f64) * 0.9)
                    .clamp(1.0, 512.0)
                    .round() as usize;
                let actual_depth: usize = worker_priority_counters
                    .iter()
                    .map(|table| table[priority].load(AtomicOrdering::Relaxed))
                    .sum();
                let target_depth = capacity.max(actual_depth.min(capacity * 2).max(1));

                match priority {
                    Priority::High => {
                        let mut remaining = target_depth.max(1);
                        let mut alloc0;
                        let mut alloc1 = 0usize;
                        let mut alloc2 = 0usize;
                        if remaining >= 3 {
                            alloc1 = 1;
                            alloc2 = 1;
                            remaining -= 2;
                        } else if remaining == 2 {
                            alloc1 = 1;
                            remaining -= 1;
                        }
                        alloc0 = remaining;
                        alloc0 = alloc0.min(HIGH_PRIMARY_MAX);
                        alloc1 = alloc1.min(HIGH_SPILL_MAX);
                        alloc2 = alloc2.min(HIGH_SPILL_MAX);
                        new_limits[0][Priority::High] = alloc0.max(1);
                        new_limits[1][Priority::High] = alloc1;
                        new_limits[2][Priority::High] = alloc2;
                        new_totals[0] += new_limits[0][Priority::High];
                        new_totals[1] += new_limits[1][Priority::High];
                        new_totals[2] += new_limits[2][Priority::High];

                        let guard_us =
                            ((avg_ns / 1_000).saturating_mul(2)).min((budget_ns / 1_000).max(1));
                        guard_slice_candidates.push((priority, guard_us));
                        self.set_guard_threshold_us(priority, 0);
                    }
                    Priority::Medium => {
                        let target = target_depth.max(1);
                        let mut w1 = target.min(MEDIUM_W1_MAX);
                        let mut w2 = target.saturating_sub(w1).min(MEDIUM_W2_MAX);
                        if target > 1 && w2 == 0 {
                            w1 = w1.saturating_sub(1);
                            w2 = 1;
                        }
                        new_limits[1][Priority::Medium] = w1;
                        new_limits[2][Priority::Medium] = w2;
                        new_totals[1] += w1;
                        new_totals[2] += w2;

                        let guard_us = ((avg_ns / 1_000).saturating_mul(2))
                            .min((budget_ns / 1_000).max(1))
                            .min(80);
                        self.set_guard_threshold_us(priority, guard_us);
                        guard_slice_candidates.push((priority, guard_us));
                    }
                    Priority::Low => {
                        let low = target_depth.max(1).min(LOW_MAX);
                        new_limits[2][Priority::Low] = low;
                        new_totals[2] += low;
                        let guard_us = ((avg_ns / 1_000).saturating_mul(2))
                            .min((budget_ns / 1_000).max(1))
                            .min(80);
                        self.set_guard_threshold_us(priority, guard_us);
                        guard_slice_candidates.push((priority, guard_us));
                    }
                    Priority::BestEffort => {}
                }
            }

            // Ensure best-effort has a reasonable default capacity.
            new_limits[2][Priority::BestEffort] = BEST_EFFORT_MAX;
            new_totals[2] += BEST_EFFORT_MAX;

            for (worker_id, total) in new_totals.iter().enumerate() {
                let min_total = match worker_id {
                    0 => 4,
                    1 => 16,
                    _ => 64,
                };
                self.set_total_limit(worker_id, (*total).max(min_total));
                for priority in Priority::ALL {
                    let value = new_limits[worker_id][priority];
                    self.set_priority_limit(worker_id, priority, value);
                }
            }

            if let Some((_, best_guard)) = guard_slice_candidates
                .into_iter()
                .min_by_key(|(_, guard)| *guard)
            {
                self.set_guard_slice_us(best_guard.max(5).min(50));
            }
        }
    }
}

/// Multi-worker EDF scheduler with an adaptive load balancer.
///
/// Each worker maintains its own EDF heap; the shared [`AdaptiveController`] nudges backlog limits
/// and guard timings so that observed processing costs stay within the configured latency budgets.
pub struct MultiWorkerScheduler {
    input_queues: PriorityTable<Arc<Receiver<Packet>>>,
    completion: Arc<CompletionRouter>,
    worker_backlogs: Vec<Arc<AtomicUsize>>,
    worker_priority_counters: Vec<PriorityTable<Arc<AtomicUsize>>>,
    adaptive: Arc<AdaptiveController>,
    expected_latencies: Arc<PriorityTable<Duration>>,
}

#[derive(Debug, Clone)]
pub struct MultiWorkerStats {
    pub worker_queue_depths: Vec<usize>,
    pub worker_queue_capacities: Vec<usize>,
    pub dispatcher_backlog: usize,
    pub worker_priority_depths: Vec<Vec<usize>>,
}

impl MultiWorkerScheduler {
    /// Create a new multi-worker EDF scheduler with adaptive balancing.
    pub fn new(
        input_queues: PriorityTable<Arc<Receiver<Packet>>>,
        output_queues: PriorityTable<Sender<Packet>>,
        expected_latencies: PriorityTable<Duration>,
    ) -> Self {
        let worker_backlogs = WORKER_ASSIGNMENTS
            .iter()
            .map(|_| Arc::new(AtomicUsize::new(0)))
            .collect();
        let worker_priority_counters = WORKER_ASSIGNMENTS
            .iter()
            .map(|_| PriorityTable::from_fn(|_| Arc::new(AtomicUsize::new(0))))
            .collect();

        let adaptive = Arc::new(AdaptiveController::new());
        Self {
            input_queues,
            completion: Arc::new(CompletionRouter::new(output_queues)),
            worker_backlogs,
            worker_priority_counters,
            adaptive,
            expected_latencies: Arc::new(expected_latencies),
        }
    }

    /// Spawn the EDF workers plus the background auto-balancer.
    pub fn spawn_threads(
        &self,
        running: Arc<AtomicBool>,
        priority_setter: fn(i32),
        core_setter: fn(usize),
        worker_cores: &[usize],
    ) {
        let core_list: Vec<usize> = if worker_cores.is_empty() {
            vec![0]
        } else {
            worker_cores.to_vec()
        };

        for (worker_id, assignments) in WORKER_ASSIGNMENTS.iter().enumerate() {
            let input_clone = self.input_queues.clone();
            let completion = self.completion.clone();
            let running_clone = running.clone();
            let backlog = self.worker_backlogs[worker_id].clone();
            let priorities: Vec<Priority> = assignments.to_vec();
            let priority_counters = PriorityTable::from_fn(|priority| {
                self.worker_priority_counters[worker_id][priority].clone()
            });
            let controller = self.adaptive.clone();
            let core_id = core_list[worker_id % core_list.len()];
            thread::Builder::new()
                .name(format!("EDF-Worker-{}", worker_id))
                .spawn(move || {
                    priority_setter(2);
                    core_setter(core_id);
                    run_worker(
                        worker_id,
                        priorities,
                        input_clone,
                        completion,
                        running_clone,
                        backlog,
                        priority_counters,
                        controller,
                    );
                })
                .expect("failed to spawn EDF worker thread");
        }

        let controller = self.adaptive.clone();
        let running_balancer = running.clone();
        let expected = self.expected_latencies.clone();
        let priority_counters = self.worker_priority_counters.clone();
        thread::Builder::new()
            .name("EDF-AutoBalance".to_string())
            .spawn(move || {
                controller.autobalance_loop(running_balancer, expected, priority_counters);
            })
            .expect("failed to spawn EDF auto-balancer");
    }

    pub fn get_drop_counts(&self) -> EDFDropCounters {
        EDFDropCounters {
            heap: 0,
            output: self.completion.drop_counts(),
        }
    }

    pub fn stats(&self) -> MultiWorkerStats {
        MultiWorkerStats {
            worker_queue_depths: self
                .worker_backlogs
                .iter()
                .map(|depth| depth.load(AtomicOrdering::Relaxed))
                .collect(),
            worker_queue_capacities: WORKER_ASSIGNMENTS
                .iter()
                .enumerate()
                .map(|(worker_id, _)| self.adaptive.total_limit(worker_id))
                .collect(),
            dispatcher_backlog: 0,
            worker_priority_depths: self
                .worker_priority_counters
                .iter()
                .map(|table| {
                    Priority::ALL
                        .iter()
                        .map(|priority| table[*priority].load(AtomicOrdering::Relaxed))
                        .collect::<Vec<_>>()
                })
                .collect(),
        }
    }
}

/// Main worker loop: processes packets using EDF scheduling with adaptive quotas.
///
/// Each worker:
/// 1. Drains input queues (respecting per-priority and global quotas)
/// 2. Selects the earliest-deadline packet from its heap
/// 3. Checks for preemption opportunities (HIGH priority packets)
/// 4. Applies guard windows for Medium/Low packets (deadline-based aging)
/// 5. Simulates processing (busy-wait)
/// 6. Completes the packet through the sequence tracker
///
/// The worker maintains its own EDF heap and respects quotas set by the adaptive controller.
///
/// # Arguments
/// * `worker_id` - Unique identifier for this worker (0, 1, or 2)
/// * `priorities` - List of priorities this worker can process (from WORKER_ASSIGNMENTS)
/// * `input_queues` - Per-priority input channels from ingress DRR
/// * `completion` - Router for completing packets (handles sequence tracking)
/// * `running` - Atomic flag to signal shutdown
/// * `backlog` - Atomic counter for this worker's queue depth (for metrics)
/// * `shared_priority_counters` - Per-priority atomic counters (shared across workers, for adaptive controller)
/// * `controller` - Adaptive controller that sets quotas and guard timings
fn run_worker(
    worker_id: usize, // Worker identifier (0=HIGH only, 1=HIGH+MED, 2=all)
    priorities: Vec<Priority>, // Priorities this worker can process
    input_queues: PriorityTable<Arc<Receiver<Packet>>>, // Input channels per priority
    completion: Arc<CompletionRouter>, // Router for packet completion (sequence tracking)
    running: Arc<AtomicBool>, // Shutdown flag (checked each iteration)
    backlog: Arc<AtomicUsize>, // Atomic counter for worker queue depth (metrics)
    shared_priority_counters: PriorityTable<Arc<AtomicUsize>>, // Per-priority counters (adaptive controller)
    controller: Arc<AdaptiveController>, // Adaptive controller (quotas, guard timings)
) {
    // Set thread name for debugging/monitoring (platform-specific)
    let name = format!("EDF-Worker-{}", worker_id);
    if let Ok(name_cstr) = std::ffi::CString::new(name.clone()) {
        #[cfg(target_os = "macos")]
        unsafe {
            // macOS: set thread name (helps with debugging)
            libc::pthread_setname_np(name_cstr.as_ptr());
        }
        #[cfg(target_os = "linux")]
        unsafe {
            // Linux: set thread name (helps with debugging)
            libc::pthread_setname_np(libc::pthread_self(), name_cstr.as_ptr());
        }
    }

    // Each worker maintains its own min-deadline heap (BinaryHeap with reversed ordering).
    // The heap stores ScheduledItems ordered by deadline (earliest deadline at top).
    let mut heap: BinaryHeap<ScheduledItem> = BinaryHeap::new();
    // counts => number of packets enqueued for each priority inside the worker heap.
    // Used to enforce per-priority quotas (limits how many packets of each priority can be queued).
    let mut counts = PriorityTable::from_fn(|_| 0usize);
    // total_count => total packets currently admitted for that worker.
    // Used to enforce global worker backlog limit (prevents runaway heap growth).
    let mut total_count = 0usize;

    // Main worker loop: continues until shutdown signal
    while running.load(AtomicOrdering::Relaxed) {
        // Get current quotas from adaptive controller (updated every 100ms)
        let total_limit = controller.total_limit(worker_id); // Global worker backlog limit
        let per_priority_limit = controller.priority_limits(worker_id); // Per-priority quotas

        // ========================================================================
        // STEP 1: Drain input queues while respecting quotas
        // ========================================================================
        // Drain all eligible input queues while respecting per-priority + global quotas.
        // Each admitted packet receives an EDF deadline and is inserted into the heap.
        // We iterate through priorities in the order defined by WORKER_ASSIGNMENTS.
        for &priority in &priorities {
            // Loop until queue is empty or quota is reached
            loop {
                // Check if we've reached the quota for this priority or the global limit
                if counts[priority] >= per_priority_limit[priority] || total_count >= total_limit {
                    break; // Quota reached, stop draining this priority
                }
                // Try to receive a packet from the input queue (non-blocking)
                match input_queues[priority].try_recv() {
                    Ok(packet) => {
                        // Calculate absolute deadline: ingress timestamp + latency budget
                        let deadline = packet.timestamp + packet.latency_budget;
                        // Prepare work item: assign sequence number for FIFO ordering
                        let work_item = completion.prepare(priority, packet, deadline);
                        // Attempt to push into heap (respects quotas)
                        let pushed = push_with_capacity(
                            &mut heap, // The EDF min-deadline heap
                            &mut counts, // Per-priority counts (updated if pushed)
                            &mut total_count, // Total count (updated if pushed)
                            &per_priority_limit, // Quota limits
                            total_limit, // Global limit
                            ScheduledItem {
                                deadline, // EDF deadline (for heap ordering)
                                work_item, // Packet + priority + sequence
                            },
                        );
                        if pushed {
                            // Successfully pushed: update shared counter (for adaptive controller)
                            shared_priority_counters[priority]
                                .fetch_add(1, AtomicOrdering::Relaxed);
                        } else {
                            // Push failed (quota reached): stop draining this priority
                            break;
                        }
                    }
                    Err(TryRecvError::Empty) => break, // Queue empty, move to next priority
                    Err(TryRecvError::Disconnected) => break, // Channel closed, move to next priority
                }
            }
        }

        // Update backlog so metrics/GUI can display per-worker queue depth.
        // This is read by the metrics collector to show worker load in the GUI.
        backlog.store(total_count, AtomicOrdering::Relaxed);

        // ========================================================================
        // STEP 2: Select earliest-deadline packet from heap
        // ========================================================================
        // Select the earliest-deadline item. Empty heap => worker idles (yields).
        // The heap is a min-heap on deadline, so pop() returns the earliest deadline.
        let mut scheduled = match heap.pop() {
            Some(item) => item, // Found a packet to process
            None => {
                // Heap is empty: no work available, yield CPU and continue
                thread::yield_now();
                continue; // Go back to step 1 (drain queues)
            }
        };
        // Remember the priority of the selected packet (for preemption logic)
        let initial_priority = scheduled.work_item.priority;
        // Sanity check: we should have at least one packet of this priority
        debug_assert!(counts[initial_priority] > 0);
        // Update local counters: packet is no longer in the heap
        counts[initial_priority] -= 1; // Decrement per-priority count
        total_count -= 1; // Decrement total count
        // Update shared counter: adaptive controller tracks backlog
        shared_priority_counters[initial_priority].fetch_sub(1, AtomicOrdering::Relaxed);

        // ========================================================================
        // STEP 3: Immediate preemption check for HIGH priority packets
        // ========================================================================
        // Immediate preemption opportunity: before processing, check for freshly-arrived HIGH
        // packets. This minimizes the extra latency a burst incurs (they can displace the
        // current job instantly instead of waiting for the busy loop to complete).
        //
        // Skip this check if:
        // - Worker doesn't handle HIGH priority
        // - Current packet is MEDIUM and HIGH backlog already exists (avoid redundant checks)
        if priorities.contains(&Priority::High)
            && !(scheduled.work_item.priority == Priority::Medium && counts[Priority::High] > 0)
        {
            // Poll HIGH priority queue for urgent packets
            loop {
                // Check if we've reached HIGH quota or global limit
                if counts[Priority::High] >= per_priority_limit[Priority::High]
                    || total_count >= total_limit
                {
                    break; // Quota reached, stop checking
                }
                // Try to receive a HIGH priority packet (non-blocking)
                match input_queues[Priority::High].try_recv() {
                    Ok(packet) => {
                        // Calculate deadline for the HIGH packet
                        let deadline = packet.timestamp + packet.latency_budget;
                        // Prepare work item (assign sequence number)
                        let work_item = completion.prepare(Priority::High, packet, deadline);

                        // Check if this HIGH packet has an earlier deadline than the current packet
                        if deadline < scheduled.deadline {
                            // Yes: preempt! Requeue the current job and run HIGH instead.
                            let requeued_priority = initial_priority; // Remember original priority
                            // Put the current packet back into the heap
                            let requeued = push_with_capacity(
                                &mut heap, // Put back into heap
                                &mut counts, // Update counts
                                &mut total_count, // Update total
                                &per_priority_limit, // Quota limits
                                total_limit, // Global limit
                                scheduled, // The packet we're preempting
                            );
                            // This should always succeed (we just removed it, so quota should allow it)
                            debug_assert!(requeued, "failed to requeue preempted packet");
                            // Update shared counter for the requeued packet
                            shared_priority_counters[requeued_priority]
                                .fetch_add(1, AtomicOrdering::Relaxed);

                            // Replace scheduled packet with the HIGH priority one
                            scheduled = ScheduledItem {
                                deadline, // Earlier deadline
                                work_item, // HIGH priority packet
                            };
                        } else {
                            // No: HIGH packet has later deadline, just queue it normally
                            if push_with_capacity(
                                &mut heap, // Add to heap
                                &mut counts, // Update counts
                                &mut total_count, // Update total
                                &per_priority_limit, // Quota limits
                                total_limit, // Global limit
                                ScheduledItem {
                                    deadline, // Later deadline (will be processed after current)
                                    work_item, // HIGH priority packet
                                },
                            ) {
                                // Successfully queued: update shared counter
                                shared_priority_counters[Priority::High]
                                    .fetch_add(1, AtomicOrdering::Relaxed);
                            } else {
                                // Queue failed (quota reached): stop checking
                                break;
                            }
                        }
                    }
                    Err(TryRecvError::Empty) => break, // No HIGH packets available
                    Err(TryRecvError::Disconnected) => break, // Channel closed
                }
            }
        }

        // ========================================================================
        // STEP 4: Guard window (deadline-based aging) for Medium/Low packets
        // ========================================================================
        // Guard window based on "deadline aging": limit short waits to Medium/Low packets that are
        // still early in their latency budget. This keeps HIGH latency tails low without stalling
        // classes that are already close to their own deadline.
        //
        // Only applies to workers that can handle HIGH priority (they can preempt Medium/Low).
        // The guard allows Medium/Low packets to briefly wait for HIGH packets if they're still
        // early in their latency budget (e.g., first 80% of budget). Once they've consumed too
        // much of their budget, they proceed immediately to avoid missing their own deadline.
        if priorities.contains(&Priority::High) {
            // Get the packet we're about to process
            let packet = &scheduled.work_item.packet;
            // Calculate how much time has elapsed since the packet entered the pipeline
            let elapsed = Instant::now().saturating_duration_since(packet.timestamp);
            // Check if this priority has a guard threshold (Medium/Low do, High/BestEffort don't)
            if let Some(guard_threshold) =
                controller.guard_threshold(scheduled.work_item.priority, packet.latency_budget)
            {
                // Only apply guard if packet is still early in its budget
                if elapsed < guard_threshold {
                    // Track elapsed time as we loop (may be updated if we wait)
                    let mut current_elapsed = elapsed;
                    // Guard loop: continue until threshold is reached or HIGH packet arrives
                    'guard: while current_elapsed < guard_threshold {
                        // Calculate remaining guard time (how much we can still wait)
                        let remaining_guard = guard_threshold
                            .checked_sub(current_elapsed) // Time left before threshold
                            .unwrap_or_default(); // Default to zero if subtraction underflows
                        // If no time remaining, exit immediately
                        if remaining_guard.is_zero() {
                            break; // Threshold reached, proceed with processing
                        }
                        // Limit each guard slice to a small duration (e.g., 10-50 µs)
                        // This prevents long blocking waits and allows periodic re-evaluation
                        let guard_window = remaining_guard.min(controller.guard_slice());
                        // Calculate deadline for this guard slice
                        let guard_deadline = Instant::now() + guard_window;
                        // Spin-wait for HIGH packets during this guard slice
                        while Instant::now() < guard_deadline {
                            // Check if we've reached HIGH quota or global limit
                            if counts[Priority::High] >= per_priority_limit[Priority::High]
                                || total_count >= total_limit
                            {
                                break; // Quota reached, stop waiting
                            }
                            // Try to receive a HIGH priority packet (non-blocking)
                            match input_queues[Priority::High].try_recv() {
                                Ok(packet) => {
                                    // HIGH packet arrived! Preempt the current packet.
                                    let deadline = packet.timestamp + packet.latency_budget;
                                    // Prepare work item (assign sequence number)
                                    let work_item =
                                        completion.prepare(Priority::High, packet, deadline);

                                    // Requeue the current packet (Medium/Low) back into heap
                                    let requeued_priority = initial_priority; // Remember original priority
                                    let requeued = push_with_capacity(
                                        &mut heap, // Put back into heap
                                        &mut counts, // Update counts
                                        &mut total_count, // Update total
                                        &per_priority_limit, // Quota limits
                                        total_limit, // Global limit
                                        scheduled, // The packet we're preempting
                                    );
                                    // This should always succeed (we just removed it)
                                    debug_assert!(requeued, "failed to requeue guarded packet");
                                    // Update shared counter for the requeued packet
                                    shared_priority_counters[requeued_priority]
                                        .fetch_add(1, AtomicOrdering::Relaxed);

                                    // Replace scheduled packet with HIGH priority one
                                    scheduled = ScheduledItem {
                                        deadline, // HIGH packet deadline
                                        work_item, // HIGH priority packet
                                    };
                                    // Exit guard loop: we found a HIGH packet to process
                                    break 'guard;
                                }
                                Err(TryRecvError::Empty) => {
                                    // No HIGH packets available: spin briefly (CPU hint for low latency)
                                    std::hint::spin_loop();
                                }
                                Err(TryRecvError::Disconnected) => {
                                    // Channel closed: exit guard loop
                                    break 'guard;
                                }
                            }
                        }
                        // Update elapsed time (may have increased due to waiting)
                        current_elapsed =
                            Instant::now().saturating_duration_since(packet.timestamp);
                        // Check if we've reached the threshold (packet consumed too much budget)
                        if current_elapsed >= guard_threshold {
                            break; // Threshold reached, proceed with processing
                        }
                    }
                }
            }
        }

        // ========================================================================
        // STEP 5: Simulate packet processing (busy-wait)
        // ========================================================================
        // Simulate the CPU work (busy wait). In production this would be real compute.
        // Processing time is size-dependent: 0.05 ms base + up to 0.1 ms extra for large packets.
        let processing_time = processing_duration(&scheduled.work_item.packet);
        let start = Instant::now();
        // Busy-wait loop: spin until processing time has elapsed
        while start.elapsed() < processing_time {
            std::hint::spin_loop(); // CPU hint: indicates tight spin loop
        }
        // Record processing time for adaptive controller (updates EMA)
        let priority = scheduled.work_item.priority;
        controller.observe_processing_time(priority, processing_time);

        // ========================================================================
        // STEP 6: Complete packet through sequence tracker
        // ========================================================================
        // Emit the packet through the completion router. Sequence tracking guarantees
        // per-priority FIFO order even though multiple workers run in parallel.
        // The completion router will:
        // 1. Check if the packet's sequence number matches the expected next sequence
        // 2. If yes: send immediately and check for consecutive pending packets
        // 3. If no: buffer the packet until its sequence arrives
        completion.complete(scheduled.work_item);
    }

    // Shutdown: clear backlog counter (worker is stopping)
    backlog.store(0, AtomicOrdering::Relaxed);
}
