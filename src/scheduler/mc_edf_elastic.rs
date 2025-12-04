//! Multi-Channel EDF with Elasticity (MC-EDF) Scheduler
//!
//! Implements a multi-channel EDF scheduler where each priority class has its own EDF scheduler.
//! HIGH priority packets are distributed intelligently across all three EDF channels using a
//! Round Robin that balances both packet count and deadline distribution.
//!
//! Architecture:
//! 1. Three separate EDF schedulers (one per priority: HIGH, MEDIUM, LOW)
//! 2. Intelligent Round Robin distributes HIGH packets across all three EDFs
//! 3. Each EDF (except HIGH) has elasticity of 0.1ms when only MEDIUM/LOW packets are present
//! 4. Re-ordering stage combines outputs from all three EDFs
//!
//! The intelligent Round Robin for HIGH priority:
//! - Distributes packets evenly by count across the three EDFs
//! - Considers deadlines to balance urgency across EDFs
//! - Ensures fair distribution of HIGH priority load

// Import packet representation used throughout the pipeline
use crate::packet::Packet;
// Import priority classes and helper table structure
use crate::priority::{Priority, PriorityTable};
// Import drop counter structure for metrics compatibility
use crate::scheduler::edf::EDFDropCounters;
// Import processing duration calculation
use crate::scheduler::multi_worker_edf::processing_duration;
// Import lock-free channels for inter-thread communication
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender, TryRecvError};
// Import mutex for synchronization
use parking_lot::Mutex;
// Import ordering trait for deadline comparisons
use std::cmp::Ordering;
// Import BinaryHeap for EDF min-heap (for MEDIUM/LOW EDFs)
use std::collections::BinaryHeap;
// Import VecDeque for FIFO queue (for HIGH EDF - O(1) operations)
use std::collections::VecDeque;
// Import atomic types for lock-free flags and counters
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
// Import Arc for shared ownership
use std::sync::Arc;
// Import thread utilities for spawning worker threads
use std::thread;
// Import time types for deadlines and elasticity
use std::time::{Duration, Instant};

/// Task in an EDF scheduler, ordered by virtual deadline (earliest first).
#[derive(Debug, Clone)]
struct EDFTask {
    /// The packet to process
    packet: Packet,
    /// Virtual deadline: timestamp + (latency_budget / scaling_factor)
    /// HIGH priority has scaling_factor=4 (budget divided by 4), MEDIUM has scaling_factor=2
    /// This gives HIGH and MEDIUM priority tasks earlier virtual deadlines, prioritizing them
    virtual_deadline: Instant,
    /// Real deadline: timestamp + latency_budget (used for re-ordering output)
    real_deadline: Instant,
    /// Priority class (for routing to correct output)
    priority: Priority,
}

impl Ord for EDFTask {
    /// Compare by virtual deadline in reverse order to create a min-heap.
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse comparison so BinaryHeap pops the earliest virtual deadline first.
        other.virtual_deadline.cmp(&self.virtual_deadline)
    }
}

impl PartialOrd for EDFTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for EDFTask {
    fn eq(&self, other: &Self) -> bool {
        self.virtual_deadline == other.virtual_deadline
    }
}

impl Eq for EDFTask {}

/// Statistics for intelligent Round Robin distribution of HIGH priority packets.
/// HIGH packets are distributed across 3 EDFs: HIGH, MEDIUM, LOW
struct HighDistributionStats {
    /// Number of HIGH packets distributed to each EDF channel (HIGH, MEDIUM, LOW)
    packet_count: [u64; 3],
    /// Sum of deadlines for HIGH packets in each EDF channel (for balancing urgency)
    deadline_sum: [Duration; 3],
}

impl HighDistributionStats {
    fn new() -> Self {
        Self {
            packet_count: [0; 3],
            deadline_sum: [Duration::ZERO; 3],
        }
    }

    /// Find the best EDF channel for a HIGH priority packet.
    /// Considers both packet count and deadline distribution.
    fn find_best_channel(&self, _deadline: Instant, _now: Instant) -> usize {
        // Score each channel: prefer channels with fewer packets and earlier deadlines
        let mut best_channel = 0;
        let mut best_score = i64::MAX;

        for (idx, &count) in self.packet_count.iter().enumerate() {
            let deadline_sum = self.deadline_sum[idx];

            // Score based on:
            // 1. Packet count (lower is better)
            // 2. Deadline sum (earlier deadlines = smaller sum = higher priority)
            // Weight packet count more heavily for fairness
            let count_penalty = count as i64 * 1000;
            let deadline_penalty = deadline_sum.as_micros() as i64;
            let score = count_penalty + deadline_penalty;

            if score < best_score {
                best_score = score;
                best_channel = idx;
            }
        }

        best_channel
    }

    /// Record that a HIGH packet was distributed to a channel.
    fn record_distribution(&mut self, channel: usize, deadline: Instant, now: Instant) {
        self.packet_count[channel] += 1;
        let deadline_slack = deadline.saturating_duration_since(now);
        self.deadline_sum[channel] += deadline_slack;
    }

    /// Record distribution with pre-calculated deadline slack (optimization to avoid recalculation in lock).
    ///
    /// Updates packet count and deadline sum for the channel.
    /// Use this when deadline_slack is already calculated outside the lock.
    fn record_distribution_with_slack(&mut self, channel: usize, deadline_slack: Duration) {
        self.packet_count[channel] += 1;
        self.deadline_sum[channel] += deadline_slack;
    }
}

/// EDF scheduler for a single priority channel.
/// Uses per-priority input queues to allow priority-based reception.
struct ChannelEDF {
    /// FIFO queue for HIGH priority (O(1) operations) - only used by HIGH EDF
    /// With aggressive VD scaling, HIGH packets are already ordered by VD
    /// Using FIFO instead of heap avoids O(log n) overhead and lock contention
    high_queue: Option<Arc<Mutex<VecDeque<EDFTask>>>>,
    /// Min-heap of tasks ordered by deadline (earliest first) - used by MEDIUM/LOW EDFs
    /// For HIGH EDF, heap is only used if high_queue is None (backward compatibility)
    heap: Arc<Mutex<BinaryHeap<EDFTask>>>,
    /// Per-priority input channels for this EDF (HIGH, MEDIUM, LOW, etc.)
    /// Not all priorities may be active for a given EDF
    input_queues: PriorityTable<Arc<Receiver<EDFTask>>>,
    /// Output channel for processed packets (goes to re-ordering stage)
    output: Sender<Packet>,
}

impl ChannelEDF {
    fn new(input_queues: PriorityTable<Arc<Receiver<EDFTask>>>, output: Sender<Packet>) -> Self {
        Self {
            high_queue: None, // By default, use heap for all EDFs
            heap: Arc::new(Mutex::new(BinaryHeap::new())),
            input_queues,
            output,
        }
    }

    /// Create ChannelEDF with FIFO queue for HIGH priority (optimized for HIGH EDF)
    fn new_with_fifo(
        input_queues: PriorityTable<Arc<Receiver<EDFTask>>>,
        output: Sender<Packet>,
    ) -> Self {
        Self {
            high_queue: Some(Arc::new(Mutex::new(VecDeque::new()))),
            heap: Arc::new(Mutex::new(BinaryHeap::new())), // Still needed for compatibility
            input_queues,
            output,
        }
    }
}

/// Packet with priority for re-ordering stage.
#[derive(Debug, Clone)]
struct OrderedPacket {
    /// The packet to output
    packet: Packet,
    /// Priority class
    priority: Priority,
    /// Absolute deadline for ordering
    deadline: Instant,
}

impl Ord for OrderedPacket {
    /// Compare by deadline in reverse order to create a min-heap (earliest deadline first).
    fn cmp(&self, other: &Self) -> Ordering {
        other.deadline.cmp(&self.deadline)
    }
}

impl PartialOrd for OrderedPacket {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for OrderedPacket {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline
    }
}

impl Eq for OrderedPacket {}

/// Tracks average processing latency for HIGH priority packets.
/// Used to dynamically calculate timeout for MEDIUM/LOW preemption.
struct HighLatencyTracker {
    /// Average processing latency in microseconds (updated with EMA)
    avg_latency_us: Arc<Mutex<f64>>,
}

impl HighLatencyTracker {
    fn new() -> Self {
        Self {
            avg_latency_us: Arc::new(Mutex::new(0.0)),
        }
    }

    /// Record a processing latency measurement.
    /// Uses Exponential Moving Average (EMA) with alpha = 0.1 for smooth adaptation.
    /// This method should be called by the worker after processing each HIGH packet.
    fn record_latency(tracker: &Arc<Mutex<f64>>, latency: Duration) {
        let latency_us = latency.as_micros() as f64;
        let mut avg = tracker.lock();
        if *avg == 0.0 {
            // First measurement: use it directly
            *avg = latency_us;
        } else {
            // EMA: new_avg = alpha * new_value + (1 - alpha) * old_avg
            // alpha = 0.1 gives 90% weight to history, 10% to new measurement
            let alpha = 0.1;
            *avg = alpha * latency_us + (1.0 - alpha) * *avg;
        }
    }
}

/// Tracks average inter-arrival time for HIGH priority packets.
/// Used to dynamically calculate timeout for MEDIUM/LOW preemption based on packet arrival rate.
struct HighInterArrivalTracker {
    /// Average inter-arrival time in microseconds (updated with EMA)
    avg_inter_arrival_us: Arc<Mutex<f64>>,
    /// Last arrival timestamp for calculating inter-arrival time
    last_arrival: Arc<Mutex<Option<Instant>>>,
}

impl HighInterArrivalTracker {
    fn new() -> Self {
        Self {
            avg_inter_arrival_us: Arc::new(Mutex::new(0.0)),
            last_arrival: Arc::new(Mutex::new(None)),
        }
    }

    /// Record a HIGH packet arrival and update inter-arrival time.
    /// Uses Exponential Moving Average (EMA) with alpha = 0.1 for smooth adaptation.
    /// This method should be called when a HIGH packet arrives at the worker.
    fn record_arrival(tracker: &Arc<Mutex<f64>>, last_arrival: &Arc<Mutex<Option<Instant>>>) {
        let now = Instant::now();
        let mut last = last_arrival.lock();
        let mut avg = tracker.lock();
        
        if let Some(last_time) = *last {
            let inter_arrival = now.duration_since(last_time);
            let inter_arrival_us = inter_arrival.as_micros() as f64;
            
            if *avg == 0.0 {
                // First measurement: use it directly
                *avg = inter_arrival_us;
            } else {
                // EMA: new_avg = alpha * new_value + (1 - alpha) * old_avg
                // alpha = 0.1 gives 90% weight to history, 10% to new measurement
                let alpha = 0.1;
                *avg = alpha * inter_arrival_us + (1.0 - alpha) * *avg;
            }
        }
        
        // Update last arrival time
        *last = Some(now);
    }
}

/// Multi-Channel EDF with Elasticity Scheduler
pub struct MCEDFElasticScheduler {
    /// Per-priority input channels from ingress DRR
    input_queues: PriorityTable<Arc<Receiver<Packet>>>,
    /// Per-priority output channels to egress DRR
    output_queues: PriorityTable<Sender<Packet>>,
    /// Drop counters for metrics
    drop_counters: PriorityTable<Arc<AtomicU64>>,
    /// Per-priority input channels for HIGH EDF (can receive HIGH packets)
    high_input_tx: PriorityTable<Option<Sender<EDFTask>>>,
    high_input_rx: PriorityTable<Arc<Receiver<EDFTask>>>,
    /// Per-priority input channels for MEDIUM EDF (can receive HIGH and MEDIUM packets)
    medium_input_tx: PriorityTable<Option<Sender<EDFTask>>>,
    medium_input_rx: PriorityTable<Arc<Receiver<EDFTask>>>,
    /// Per-priority input channels for LOW EDF (can receive HIGH, MEDIUM, and LOW packets)
    low_input_tx: PriorityTable<Option<Sender<EDFTask>>>,
    low_input_rx: PriorityTable<Arc<Receiver<EDFTask>>>,
    /// Statistics for intelligent HIGH distribution across 3 EDFs
    high_distribution_stats: Arc<Mutex<HighDistributionStats>>,
    /// Tracker for average HIGH priority processing latency (for dynamic timeout)
    /// Shared across all EDF workers to calculate timeout dynamically
    high_latency_tracker: HighLatencyTracker,
    /// Tracker for average HIGH priority inter-arrival time (for dynamic timeout based on arrival rate)
    /// Shared across all EDF workers to calculate timeout based on packet arrival rate
    high_inter_arrival_tracker: HighInterArrivalTracker,
    /// Internal channels: outputs from EDFs go to re-ordering stage
    /// Stored as Arc to allow cloning for worker threads
    reorder_input_rxs: [Arc<Receiver<Packet>>; 3], // HIGH, MEDIUM, LOW
    /// Output transmitters for re-ordering stage (stored for spawn_threads)
    reorder_output_txs: PriorityTable<Sender<Packet>>,
    /// Internal transmitters from EDFs to re-ordering (stored for spawn_threads)
    edf_output_txs: [Sender<Packet>; 3], // HIGH, MEDIUM, LOW
}

impl MCEDFElasticScheduler {
    /// Create a new MC-EDF scheduler.
    ///
    /// # Arguments
    /// * `input_queues` - Per-priority input channels from ingress DRR
    /// * `output_queues` - Per-priority output channels to egress DRR
    pub fn new(
        input_queues: PriorityTable<Arc<Receiver<Packet>>>,
        output_queues: PriorityTable<Sender<Packet>>,
    ) -> Self {
        let drop_counters = PriorityTable::from_fn(|_| Arc::new(AtomicU64::new(0)));

        // Create per-priority channels for each of the 3 EDFs: HIGH, MEDIUM, LOW
        // HIGH can be distributed to all 3 EDFs to avoid favoring HIGH during bursts
        // Each EDF has per-priority input queues to allow priority-based reception

        // HIGH EDF: receives HIGH packets only
        let (high_high_tx, high_high_rx) = unbounded();
        let high_input_tx = PriorityTable::from_fn(|priority| {
            if priority == Priority::High {
                Some(high_high_tx.clone())
            } else {
                None
            }
        });
        let high_input_rx = PriorityTable::from_fn(|priority| {
            if priority == Priority::High {
                Arc::new(high_high_rx.clone())
            } else {
                // Dummy receiver that will never receive (channel is dropped)
                Arc::new(unbounded().1)
            }
        });

        // MEDIUM EDF: receives HIGH and MEDIUM packets
        let (medium_high_tx, medium_high_rx) = unbounded();
        let (medium_medium_tx, medium_medium_rx) = unbounded();
        let medium_input_tx = PriorityTable::from_fn(|priority| match priority {
            Priority::High => Some(medium_high_tx.clone()),
            Priority::Medium => Some(medium_medium_tx.clone()),
            _ => None,
        });
        let medium_input_rx = PriorityTable::from_fn(|priority| {
            match priority {
                Priority::High => Arc::new(medium_high_rx.clone()),
                Priority::Medium => Arc::new(medium_medium_rx.clone()),
                _ => Arc::new(unbounded().1), // Dummy
            }
        });

        // LOW EDF: receives HIGH, MEDIUM, and LOW packets
        let (low_high_tx, low_high_rx) = unbounded();
        let (low_medium_tx, low_medium_rx) = unbounded();
        let (low_low_tx, low_low_rx) = unbounded();
        let low_input_tx = PriorityTable::from_fn(|priority| match priority {
            Priority::High => Some(low_high_tx.clone()),
            Priority::Medium => Some(low_medium_tx.clone()),
            Priority::Low => Some(low_low_tx.clone()),
            _ => None,
        });
        let low_input_rx = PriorityTable::from_fn(|priority| {
            match priority {
                Priority::High => Arc::new(low_high_rx.clone()),
                Priority::Medium => Arc::new(low_medium_rx.clone()),
                Priority::Low => Arc::new(low_low_rx.clone()),
                _ => Arc::new(unbounded().1), // Dummy
            }
        });

        // Create internal channels: EDF outputs go to re-ordering stage
        let (high_edf_out_tx, high_edf_out_rx) = unbounded();
        let (medium_edf_out_tx, medium_edf_out_rx) = unbounded();
        let (low_edf_out_tx, low_edf_out_rx) = unbounded();

        // Store receivers for the re-ordering stage (3 EDFs: HIGH, MEDIUM, LOW)
        let reorder_input_rxs = [
            Arc::new(high_edf_out_rx),
            Arc::new(medium_edf_out_rx),
            Arc::new(low_edf_out_rx),
        ];

        // Store transmitters for EDF workers (internal channels to re-ordering)
        let edf_output_txs = [high_edf_out_tx, medium_edf_out_tx, low_edf_out_tx];

        let high_distribution_stats = Arc::new(Mutex::new(HighDistributionStats::new()));
        let high_latency_tracker = HighLatencyTracker::new();
        let high_inter_arrival_tracker = HighInterArrivalTracker::new();

        Self {
            input_queues,
            output_queues: output_queues.clone(),
            drop_counters,
            high_input_tx,
            high_input_rx,
            medium_input_tx,
            medium_input_rx,
            low_input_tx,
            low_input_rx,
            high_distribution_stats,
            high_latency_tracker,
            high_inter_arrival_tracker,
            reorder_input_rxs,
            reorder_output_txs: output_queues,
            edf_output_txs,
        }
    }

    /// Return drop counters for metrics emission.
    pub fn get_drop_counts(&self) -> EDFDropCounters {
        EDFDropCounters {
            heap: 0,
            output: PriorityTable::from_fn(|priority| {
                self.drop_counters[priority].load(AtomicOrdering::Relaxed)
            }),
        }
    }

    /// Spawn dispatcher and worker threads.
    pub fn spawn_threads(
        &self,
        running: Arc<AtomicBool>,
        set_priority: fn(i32),
        set_core: fn(usize),
        dispatcher_core: usize,
        worker_cores: &[usize],
    ) {
        // Create 3 EDF channels (output to internal re-ordering channels)
        // HIGH EDF: receives HIGH packets only - use FIFO for O(1) operations
        let high_edf =
            ChannelEDF::new_with_fifo(self.high_input_rx.clone(), self.edf_output_txs[0].clone());
        // MEDIUM EDF: receives HIGH and MEDIUM packets
        let medium_edf =
            ChannelEDF::new(self.medium_input_rx.clone(), self.edf_output_txs[1].clone());
        // LOW EDF: receives HIGH, MEDIUM, and LOW packets
        let low_edf = ChannelEDF::new(self.low_input_rx.clone(), self.edf_output_txs[2].clone());

        // Spawn dispatcher thread (intelligent Round Robin for HIGH across 3 EDFs)
        let dispatcher_inputs = self.input_queues.clone();
        let dispatcher_high_tx = self.high_input_tx.clone();
        let dispatcher_medium_tx = self.medium_input_tx.clone();
        let dispatcher_low_tx = self.low_input_tx.clone();
        let dispatcher_stats = self.high_distribution_stats.clone();
        let dispatcher_running = running.clone();

        thread::Builder::new()
            .name("MC-EDF-Dispatcher".to_string())
            .spawn(move || {
                set_priority(3);
                set_core(dispatcher_core);
                dispatcher_loop(
                    dispatcher_inputs,
                    dispatcher_high_tx,
                    dispatcher_medium_tx,
                    dispatcher_low_tx,
                    dispatcher_stats,
                    dispatcher_running,
                );
            })
            .expect("failed to spawn MC-EDF dispatcher");

        // Spawn 3 EDF worker threads
        let worker_running = running.clone();

        // HIGH EDF gets dedicated core to avoid CPU contention with MEDIUM/LOW
        // MEDIUM/LOW EDFs share a core (they have larger budgets and can tolerate contention)
        let high_edf_core = worker_cores[0];
        let medium_low_edf_core = worker_cores.get(1).copied().unwrap_or(worker_cores[0]);

        // Clone latency tracker for all workers (shared across all EDFs)
        let latency_tracker = self.high_latency_tracker.avg_latency_us.clone();
        // Clone inter-arrival tracker for all workers (shared across all EDFs)
        let inter_arrival_tracker = self.high_inter_arrival_tracker.avg_inter_arrival_us.clone();
        let inter_arrival_last = self.high_inter_arrival_tracker.last_arrival.clone();

        // HIGH EDF (receives HIGH packets only) - dedicated core, uses FIFO for O(1) operations
        let high_edf_high_queue = high_edf.high_queue.clone();
        let high_edf_heap = high_edf.heap.clone();
        let high_edf_inputs = high_edf.input_queues.clone();
        let high_edf_output = high_edf.output.clone();
        let high_edf_drops = self.drop_counters[Priority::High].clone();
        let worker_running_high = worker_running.clone();
        let high_latency_tracker = latency_tracker.clone();
        let high_inter_arrival_tracker = inter_arrival_tracker.clone();
        let high_inter_arrival_last = inter_arrival_last.clone();
        thread::Builder::new()
            .name("MC-EDF-HIGH".to_string())
            .spawn(move || {
                set_priority(3);
                set_core(high_edf_core); // Dedicated core for HIGH
                edf_worker_loop(
                    high_edf_high_queue,
                    high_edf_heap,
                    high_edf_inputs,
                    high_edf_output,
                    Priority::High,
                    high_edf_drops,
                    worker_running_high,
                    Some(high_latency_tracker),
                    Some(high_inter_arrival_tracker),
                    Some(high_inter_arrival_last),
                );
            })
            .expect("failed to spawn MC-EDF HIGH worker");

        // MEDIUM EDF (receives HIGH and MEDIUM packets) - uses heap (no FIFO)
        let medium_edf_heap = medium_edf.heap.clone();
        let medium_edf_inputs = medium_edf.input_queues.clone();
        let medium_edf_output = medium_edf.output.clone();
        let medium_edf_drops = self.drop_counters[Priority::Medium].clone();
        let worker_running_medium = worker_running.clone();
        let medium_latency_tracker = latency_tracker.clone();
        let medium_inter_arrival_tracker = inter_arrival_tracker.clone();
        let medium_inter_arrival_last = inter_arrival_last.clone();
        thread::Builder::new()
            .name("MC-EDF-MEDIUM".to_string())
            .spawn(move || {
                set_priority(3);
                set_core(medium_low_edf_core); // Shared core with LOW
                edf_worker_loop(
                    None, // No FIFO for MEDIUM EDF
                    medium_edf_heap,
                    medium_edf_inputs,
                    medium_edf_output,
                    Priority::Medium,
                    medium_edf_drops,
                    worker_running_medium,
                    Some(medium_latency_tracker),
                    Some(medium_inter_arrival_tracker),
                    Some(medium_inter_arrival_last),
                );
            })
            .expect("failed to spawn MC-EDF MEDIUM worker");

        // LOW EDF (receives HIGH and LOW packets) - uses heap (no FIFO)
        let low_edf_heap = low_edf.heap.clone();
        let low_edf_inputs = low_edf.input_queues.clone();
        let low_edf_output = low_edf.output.clone();
        let low_edf_drops = self.drop_counters[Priority::Low].clone();
        let worker_running_low = worker_running.clone();
        let low_latency_tracker = latency_tracker.clone();
        let low_inter_arrival_tracker = inter_arrival_tracker.clone();
        let low_inter_arrival_last = inter_arrival_last.clone();
        thread::Builder::new()
            .name("MC-EDF-LOW".to_string())
            .spawn(move || {
                set_priority(3);
                set_core(medium_low_edf_core); // Shared core with MEDIUM
                edf_worker_loop(
                    None, // No FIFO for LOW EDF
                    low_edf_heap,
                    low_edf_inputs,
                    low_edf_output,
                    Priority::Low,
                    low_edf_drops,
                    worker_running_low,
                    Some(low_latency_tracker),
                    Some(low_inter_arrival_tracker),
                    Some(low_inter_arrival_last),
                );
            })
            .expect("failed to spawn MC-EDF LOW worker");

        // Spawn re-ordering thread (combines outputs from 3 EDFs: HIGH, MEDIUM, LOW)
        // Re-ordering also runs on the same core as EDFs
        let reorder_inputs = [
            self.reorder_input_rxs[0].clone(), // HIGH
            self.reorder_input_rxs[1].clone(), // MEDIUM
            self.reorder_input_rxs[2].clone(), // LOW
        ];
        let reorder_outputs = self.reorder_output_txs.clone();
        let reorder_drops = self.drop_counters.clone();
        let reorder_running = running.clone();

        thread::Builder::new()
            .name("MC-EDF-Reorder".to_string())
            .spawn(move || {
                set_priority(3);
                // Re-ordering runs on same core as HIGH to minimize latency for HIGH packets
                set_core(high_edf_core);
                reorder_loop(
                    reorder_inputs,
                    reorder_outputs,
                    reorder_drops,
                    reorder_running,
                );
            })
            .expect("failed to spawn MC-EDF re-ordering thread");
    }
}

/// Calculate virtual deadline scaling factor based on priority.
///
/// Virtual deadlines reduce the latency budget to prioritize higher priority tasks:
/// - HIGH: budget / 40 (factor = 0.025) - Extremely aggressive to achieve lower P50
/// - MEDIUM: budget / 2 (factor = 0.5)
/// - LOW and others: no reduction (factor = 1.0)
fn virtual_deadline_scale(priority: Priority) -> f64 {
    match priority {
        Priority::High => 0.025, // Divide by 40 (extremely aggressive for lower P50)
        Priority::Medium => 0.5, // Divide by 2
        _ => 1.0,                // No reduction
    }
}

/// Dispatcher loop: distributes packets to appropriate EDF channels.
///
/// HIGH priority packets are distributed using intelligent Round Robin across 3 EDF channels
/// (HIGH, MEDIUM, LOW) to avoid favoring HIGH during bursts.
/// MEDIUM packets go to MEDIUM EDF, LOW packets go to LOW EDF.
/// Virtual deadlines are calculated to prioritize HIGH (divide by 16) and MEDIUM (divide by 2).
/// Uses per-priority queues for each EDF.
fn dispatcher_loop(
    input_queues: PriorityTable<Arc<Receiver<Packet>>>,
    high_input_tx: PriorityTable<Option<Sender<EDFTask>>>,
    medium_input_tx: PriorityTable<Option<Sender<EDFTask>>>,
    low_input_tx: PriorityTable<Option<Sender<EDFTask>>>,
    distribution_stats: Arc<Mutex<HighDistributionStats>>,
    running: Arc<AtomicBool>,
) {
    // Pre-allocate batch vector to avoid repeated allocations
    let mut high_batch = Vec::with_capacity(32);

    while running.load(AtomicOrdering::Relaxed) {
        let now = Instant::now();

        // Batch process HIGH priority packets: collect multiple HIGH packets first, then dispatch
        // This reduces lock contention by batching distribution_stats updates
        high_batch.clear();
        let mut high_count = 0;
        while high_count < 32 {
            match input_queues[Priority::High].try_recv() {
                Ok(packet) => {
                    let real_deadline = packet.timestamp + packet.latency_budget;
                    // Calculate virtual deadline: divide budget by 40 for HIGH (very aggressive)
                    let scale = virtual_deadline_scale(Priority::High);
                    let virtual_budget = packet.latency_budget.mul_f64(scale);
                    let virtual_deadline = packet.timestamp + virtual_budget;
                    let deadline_slack = real_deadline.saturating_duration_since(now);

                    high_batch.push((
                        EDFTask {
                            packet,
                            virtual_deadline,
                            real_deadline,
                            priority: Priority::High,
                        },
                        deadline_slack,
                    ));
                    high_count += 1;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        // Dispatch batched HIGH packets: minimize lock acquisitions
        if !high_batch.is_empty() {
            // Acquire lock once for all HIGH packets in batch
            let channels: Vec<(usize, EDFTask, Duration)> = {
                let stats = distribution_stats.lock();
                high_batch
                    .iter()
                    .map(|(task, deadline_slack)| {
                        let channel = stats.find_best_channel(task.real_deadline, now);
                        (channel, task.clone(), *deadline_slack)
                    })
                    .collect()
            };
            // Lock released - send all packets without lock
            high_batch.clear(); // Clear after collecting channels

            // Send all packets and record distributions (re-acquire lock once per successful send)
            for (channel, task, deadline_slack) in channels {
                let sent = match channel {
                    0 => {
                        if let Some(ref tx) = high_input_tx[Priority::High] {
                            tx.send(task).is_ok()
                        } else {
                            false
                        }
                    }
                    1 => {
                        if let Some(ref tx) = medium_input_tx[Priority::High] {
                            tx.send(task).is_ok()
                        } else {
                            false
                        }
                    }
                    2 => {
                        if let Some(ref tx) = low_input_tx[Priority::High] {
                            tx.send(task).is_ok()
                        } else {
                            false
                        }
                    }
                    _ => false,
                };

                // Record distribution only if send succeeded (minimal lock time)
                if sent {
                    let mut stats = distribution_stats.lock();
                    stats.record_distribution_with_slack(channel, deadline_slack);
                }
            }
        }

        // Process MEDIUM priority packets (only after all HIGH are dispatched)
        if let Ok(packet) = input_queues[Priority::Medium].try_recv() {
            let real_deadline = packet.timestamp + packet.latency_budget;
            // Calculate virtual deadline: divide budget by 2 for MEDIUM
            let scale = virtual_deadline_scale(Priority::Medium);
            let virtual_budget = packet.latency_budget.mul_f64(scale);
            let virtual_deadline = packet.timestamp + virtual_budget;

            let task = EDFTask {
                packet,
                virtual_deadline,
                real_deadline,
                priority: Priority::Medium,
            };
            // Send to MEDIUM priority queue of MEDIUM EDF
            if let Some(ref tx) = medium_input_tx[Priority::Medium] {
                let _ = tx.send(task);
            }
        }

        // Process LOW priority packets (only after all HIGH and MEDIUM are dispatched)
        if let Ok(packet) = input_queues[Priority::Low].try_recv() {
            let real_deadline = packet.timestamp + packet.latency_budget;
            // No virtual deadline reduction for LOW
            let scale = virtual_deadline_scale(Priority::Low);
            let virtual_budget = packet.latency_budget.mul_f64(scale);
            let virtual_deadline = packet.timestamp + virtual_budget;

            let task = EDFTask {
                packet,
                virtual_deadline,
                real_deadline,
                priority: Priority::Low,
            };
            // Send to LOW priority queue of LOW EDF
            if let Some(ref tx) = low_input_tx[Priority::Low] {
                let _ = tx.send(task);
            }
        }

        // Process BEST_EFFORT priority packets (if any)
        if let Ok(packet) = input_queues[Priority::BestEffort].try_recv() {
            // BEST_EFFORT can go to LOW EDF or be handled separately
            let real_deadline = packet.timestamp + packet.latency_budget;
            // No virtual deadline reduction for BEST_EFFORT
            let scale = virtual_deadline_scale(Priority::BestEffort);
            let virtual_budget = packet.latency_budget.mul_f64(scale);
            let virtual_deadline = packet.timestamp + virtual_budget;

            let task = EDFTask {
                packet,
                virtual_deadline,
                real_deadline,
                priority: Priority::BestEffort,
            };
            // BEST_EFFORT can go to LOW priority queue of LOW EDF (if supported)
            // For now, drop it or send to LOW if LOW EDF supports BEST_EFFORT
            // This is a placeholder - BEST_EFFORT handling may need adjustment
            if let Some(ref tx) = low_input_tx[Priority::Low] {
                let _ = tx.send(task);
            }
        }

        // Yield if no work
        thread::yield_now();
    }
}

/// EDF worker loop for a single channel.
///
/// Processes tasks from the EDF heap (or FIFO queue for HIGH EDF). Before processing MEDIUM/LOW tasks,
/// waits up to avg_processing_time * 1.1 for HIGH priority packets to arrive (timeout-based preemption).
fn edf_worker_loop(
    high_queue: Option<Arc<Mutex<VecDeque<EDFTask>>>>, // FIFO queue for HIGH EDF (O(1) operations)
    heap: Arc<Mutex<BinaryHeap<EDFTask>>>,             // Heap for MEDIUM/LOW EDFs or fallback
    input_queues: PriorityTable<Arc<Receiver<EDFTask>>>,
    output: Sender<Packet>,
    _priority: Priority,
    drop_counter: Arc<AtomicU64>,
    running: Arc<AtomicBool>,
    latency_tracker: Option<Arc<Mutex<f64>>>,
    inter_arrival_tracker: Option<Arc<Mutex<f64>>>,
    inter_arrival_last: Option<Arc<Mutex<Option<Instant>>>>,
) {
    // Pre-allocate received_tasks Vec to avoid repeated allocations
    let mut received_tasks = Vec::with_capacity(16);
    
    while running.load(AtomicOrdering::Relaxed) {
        // Receive new tasks from per-priority input queues (prioritize HIGH)
        // If HIGH arrives, process it immediately without batching for lower latency
        // Use try_recv to avoid blocking and reduce lock contention
        received_tasks.clear();

        // Optimization #7: For HIGH EDF with FIFO, check if queue is non-empty before reception
        // If FIFO has tasks, skip reception checks and go directly to processing
        // This avoids unnecessary try_recv() calls when FIFO already has work
        let mut skip_reception = false;
        let fifo_has_tasks = if let Some(ref fifo) = high_queue {
            let fifo_guard = fifo.lock();
            let has_tasks = !fifo_guard.is_empty();
            // Lock released here
            has_tasks
        } else {
            false
        };
        
        if fifo_has_tasks {
            // FIFO has tasks, skip reception and go directly to processing
            skip_reception = true;
        }

        // Quick path: check HIGH priority queue first (unless skip_reception is true)
        let mut high_received = false;
        if !skip_reception {
            if let Ok(task) = input_queues[Priority::High].try_recv() {
                // HIGH arrived: record inter-arrival time
                if let (Some(ref tracker), Some(ref last)) = (inter_arrival_tracker.as_ref(), inter_arrival_last.as_ref()) {
                    HighInterArrivalTracker::record_arrival(tracker, last);
                }
                // HIGH arrived: use FIFO queue if available (O(1)), otherwise use heap
                if let Some(ref fifo) = high_queue {
                    let mut fifo_guard = fifo.lock();
                    fifo_guard.push_back(task);
                    // Lock released here
                } else {
                    let mut heap_guard = heap.lock();
                    heap_guard.push(task);
                    drop(heap_guard); // Release lock immediately
                }
                high_received = true;
            }
        }

        // If HIGH was not received, check other priority queues in priority order
        if !high_received {
            // Check MEDIUM priority queue (only for MEDIUM and LOW EDFs)
            while let Ok(task) = input_queues[Priority::Medium].try_recv() {
                received_tasks.push(task);
            }

            // Check LOW priority queue (only for LOW EDF)
            while let Ok(task) = input_queues[Priority::Low].try_recv() {
                received_tasks.push(task);
            }

            // Check BEST_EFFORT priority queue
            while let Ok(task) = input_queues[Priority::BestEffort].try_recv() {
                received_tasks.push(task);
            }

            // Batch push non-HIGH tasks to heap (single lock acquisition)
            if !received_tasks.is_empty() {
                let mut heap_guard = heap.lock();
                // Use drain() to consume tasks without moving the Vec (allows reuse)
                for task in received_tasks.drain(..) {
                    heap_guard.push(task);
                }
                // Lock released here
            }
        }

        // Process task with earliest virtual deadline (HIGH will have earliest due to VD)
        // Use FIFO for HIGH EDF (O(1)) or heap for MEDIUM/LOW EDFs (O(log n))
        // Minimize lock hold time by only holding lock for pop operation
        // Optimization #7: If skip_reception was true, we know FIFO has tasks, go directly to pop
        let task = if skip_reception {
            // We already know FIFO has tasks (checked above), use it directly
            if let Some(ref fifo) = high_queue {
                let mut fifo_guard = fifo.lock();
                fifo_guard.pop_front()
            } else {
                // This shouldn't happen (skip_reception only true if high_queue is Some)
                None
            }
        } else if let Some(ref fifo) = high_queue {
            // FIFO queue for HIGH EDF: O(1) pop_front
            let mut fifo_guard = fifo.lock();
            fifo_guard.pop_front()
        } else {
            // Heap for MEDIUM/LOW EDFs: O(log n) pop
            let mut heap_guard = heap.lock();
            heap_guard.pop()
        };

        if let Some(task) = task {
            // If task is MEDIUM/LOW, wait up to avg_inter_arrival_time for HIGH priority before processing
            // This allows HIGH to preempt MEDIUM/LOW based on expected arrival rate
            if task.priority != Priority::High {
                // Calculate timeout based on average HIGH inter-arrival time
                let timeout = if let Some(ref tracker) = inter_arrival_tracker {
                    let avg = tracker.lock();
                    let avg_us = if *avg > 0.0 { *avg } else { 200.0 }; // Default 200µs if no data
                    Duration::from_micros(avg_us.min(50.0) as u64)
                } else {
                    Duration::from_micros(50) // Default: 200µs
                };
                // Use recv_timeout to wait for HIGH packets without busy polling
                // Check HIGH priority queue first
                match input_queues[Priority::High].recv_timeout(timeout) {
                    Ok(new_task) => {
                        // HIGH packet arrived! Record inter-arrival time
                        if let (Some(ref tracker), Some(ref last)) = (inter_arrival_tracker.as_ref(), inter_arrival_last.as_ref()) {
                            HighInterArrivalTracker::record_arrival(tracker, last);
                        }
                        // HIGH packet arrived! Push it and current task back to queue/heap
                        // Minimize lock hold time
                        if let Some(ref fifo) = high_queue {
                            // Use FIFO for HIGH EDF
                            let mut fifo_guard = fifo.lock();
                            fifo_guard.push_back(new_task);
                            fifo_guard.push_back(task.clone()); // Clone to keep original for potential processing
                                                                // Lock released here
                        } else {
                            // Use heap for MEDIUM/LOW EDFs
                            let mut heap_guard = heap.lock();
                            heap_guard.push(new_task);
                            heap_guard.push(task.clone()); // Clone to keep original for potential processing
                                                           // Lock released here
                        }
                        // HIGH arrived, skip processing current MEDIUM/LOW task
                        // Restart loop to process HIGH from heap
                        continue;
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        // Timeout expired, no HIGH packet arrived
                        // Continue to process the MEDIUM/LOW task
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        // Channel disconnected, continue to process current task
                    }
                }
            }

            // Process the packet (simulate processing)
            let processing_time = processing_duration(&task.packet);
            let process_start = Instant::now();

            // Use sleep instead of busy polling to avoid wasting CPU
            // For very short processing times (< 100µs), we still do a quick busy wait for accuracy
            if processing_time < Duration::from_micros(100) {
                while process_start.elapsed() < processing_time {
                    std::hint::spin_loop();
                }
            } else {
                // For longer processing times, use sleep to save CPU
                thread::sleep(processing_time);
            }

            // Record processing latency for HIGH priority packets (for dynamic timeout calculation)
            if task.priority == Priority::High {
                let actual_processing = process_start.elapsed();
                if let Some(ref tracker) = latency_tracker {
                    HighLatencyTracker::record_latency(tracker, actual_processing);
                }
            }

            // Forward to output queue
            if output.try_send(task.packet).is_err() {
                drop_counter.fetch_add(1, AtomicOrdering::Relaxed);
            }
        } else {
            thread::yield_now();
        }
    }
}

/// Re-ordering loop: combines outputs from 3 EDFs and re-orders by deadline.
///
/// Receives packets from 3 EDF channels (HIGH, MEDIUM, LOW), maintains head-of-line
/// packets per channel, and outputs packets to the appropriate priority queue in deadline order.
fn reorder_loop(
    inputs: [Arc<Receiver<Packet>>; 3], // HIGH, MEDIUM, LOW
    outputs: PriorityTable<Sender<Packet>>,
    drop_counters: PriorityTable<Arc<AtomicU64>>,
    running: Arc<AtomicBool>,
) {
    // Buffer one packet per input channel (head-of-line)
    // Index 0: HIGH EDF, Index 1: MEDIUM EDF, Index 2: LOW EDF
    let mut buffers: [Option<OrderedPacket>; 3] = [None, None, None];
    // Priority mapping: 0=HIGH, 1=MEDIUM, 2=LOW
    let priorities = [Priority::High, Priority::Medium, Priority::Low];

    while running.load(AtomicOrdering::Relaxed) {
        // Fill empty buffers from input channels
        // Yield frequently to avoid CPU contention with HIGH EDF (same core)
        let mut filled_any = false;
        for (idx, input) in inputs.iter().enumerate() {
            if buffers[idx].is_none() {
                if let Ok(packet) = input.try_recv() {
                    // Use real deadline (not virtual) for re-ordering output
                    // This ensures packets are re-ordered by their actual deadlines
                    let deadline = packet.timestamp + packet.latency_budget;
                    buffers[idx] = Some(OrderedPacket {
                        packet,
                        priority: priorities[idx],
                        deadline,
                    });
                    filled_any = true;
                }
            }
        }

        // Yield after checking channels to reduce CPU contention with HIGH EDF
        // This allows HIGH EDF to run more frequently on the same core
        if !filled_any {
            thread::yield_now();
        }

        // Find earliest deadline among buffered packets
        let mut earliest: Option<(usize, Instant)> = None;
        for (idx, buffer) in buffers.iter().enumerate() {
            if let Some(ref ordered) = buffer {
                match earliest {
                    None => earliest = Some((idx, ordered.deadline)),
                    Some((_, current_deadline)) if ordered.deadline < current_deadline => {
                        earliest = Some((idx, ordered.deadline));
                    }
                    _ => {}
                }
            }
        }

        // Process earliest packet
        if let Some((idx, _)) = earliest {
            let ordered = buffers[idx].take().expect("buffer must contain packet");
            let priority = ordered.priority;

            // Forward to appropriate output queue
            if outputs[priority].try_send(ordered.packet).is_err() {
                drop_counters[priority].fetch_add(1, AtomicOrdering::Relaxed);
            }

            // Yield after forwarding HIGH priority to allow HIGH EDF to run
            // This reduces CPU contention on the shared core
            if priority == Priority::High {
                thread::yield_now();
            }
        } else {
            // No packets available, yield to allow HIGH EDF to run
            thread::yield_now();
        }
    }
}
