//! Clairvoyant Earliest Deadline First (C-EDF) Scheduler
//!
//! Implements a clairvoyant EDF scheduler that uses knowledge of task processing times and deadlines
//! to make optimal scheduling decisions in a multi-core context. The scheduler "looks ahead" at
//! incoming tasks and assigns them to workers in a way that minimizes deadline misses.
//!
//! Algorithm:
//! 1. Dispatcher: reads packets from input queues, calculates slack time (deadline - current_time - estimated_processing_time),
//!    and assigns tasks to workers based on optimal load balancing and slack time
//! 2. Workers: process tasks from their assigned queue, maintaining per-priority FIFO ordering
//! 3. Clairvoyance: dispatcher considers multiple tasks ahead and assigns them optimally to minimize deadline misses
//!
//! The clairvoyant approach uses estimated processing times to calculate slack time, allowing the
//! dispatcher to make informed decisions about which worker should process which task to minimize
//! deadline misses across all priorities.

// Import packet representation used throughout the pipeline
use crate::packet::Packet;
// Import priority classes and helper table structure
use crate::priority::{Priority, PriorityTable};
// Import drop counter structure for metrics compatibility
use crate::scheduler::edf::EDFDropCounters;
// Import processing duration calculation (size-dependent)
use crate::scheduler::multi_worker_edf::processing_duration;
// Import lock-free channels for inter-thread communication
use crossbeam_channel::{Receiver, Sender, TryRecvError};
// Import mutex for worker queue synchronization
use parking_lot::Mutex;
// Import ordering trait for slack time comparisons
use std::cmp::Ordering;
// Import BinaryHeap for min-heap on slack time
use std::collections::BinaryHeap;
// Import atomic types for lock-free flags and counters
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
// Import Arc for shared ownership
use std::sync::Arc;
// Import thread utilities for spawning worker threads
use std::thread;
// Import time types for deadlines and slack calculations
use std::time::{Duration, Instant};
// Import collections for latency history tracking
use std::collections::VecDeque;

/// Task queued in a worker's queue, ordered by slack time (least slack first).
///
/// Slack time is calculated as: `deadline - current_time - estimated_processing_time`
/// Tasks with negative or small slack time are urgent and should be processed first.
/// The worker queue is a min-heap on slack time, so the task with least slack is always at the top.
#[derive(Debug, Clone)]
struct QueuedTask {
    /// Slack time: deadline - current_time - estimated_processing_time
    /// Negative slack means the task will likely miss its deadline
    /// Smaller slack means the task is more urgent
    slack_time: Duration,
    /// Absolute deadline: timestamp + latency_budget (when the packet must be completed)
    deadline: Instant,
    /// Priority class of the packet (for routing to correct output queue)
    priority: Priority,
    /// The packet to process
    packet: Packet,
    /// Estimated processing time for this packet (based on size)
    estimated_processing: Duration,
}

impl Ord for QueuedTask {
    /// Compare by slack time in reverse order to create a min-heap.
    ///
    /// BinaryHeap is a max-heap by default, so we reverse the comparison:
    /// - `other.slack_time.cmp(&self.slack_time)` means smaller slack times are "greater"
    /// - This makes the task with least slack (most urgent) rise to the top of the heap
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse comparison so BinaryHeap pops the least slack time first.
        // Smaller slack times are "greater" in this ordering (for min-heap behavior)
        other.slack_time.cmp(&self.slack_time)
    }
}

impl PartialOrd for QueuedTask {
    /// Delegates to `Ord::cmp` for total ordering.
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for QueuedTask {
    /// Two tasks are equal if they have the same slack time.
    fn eq(&self, other: &Self) -> bool {
        self.slack_time == other.slack_time
    }
}

impl Eq for QueuedTask {}

/// Worker queue protected by mutex.
///
/// Each worker has its own queue (min-heap on slack time). The dispatcher assigns
/// tasks to workers based on optimal load balancing and slack time considerations.
struct WorkerQueue {
    /// Min-heap of tasks ordered by slack time (least slack first)
    /// Protected by mutex to allow concurrent push/pop operations
    heap: Mutex<BinaryHeap<QueuedTask>>,
    /// Current estimated completion time for this worker
    /// Used by dispatcher to make optimal task assignments
    estimated_completion: Mutex<Instant>,
}

impl WorkerQueue {
    /// Create a new worker queue with empty heap.
    fn new() -> Self {
        Self {
            // Initialize empty heap (will be populated by dispatcher)
            heap: Mutex::new(BinaryHeap::new()),
            // Initialize estimated completion time to now
            estimated_completion: Mutex::new(Instant::now()),
        }
    }

    /// Push a task into the worker queue and update estimated completion time.
    ///
    /// # Arguments
    /// * `task` - The task to enqueue (contains packet, priority, slack time, deadline)
    fn push(&self, task: QueuedTask) {
        let mut guard = self.heap.lock();
        guard.push(task.clone()); // Push task (heap maintains min-slack-time order)

        // Update estimated completion time: current completion + task processing time
        let mut completion_guard = self.estimated_completion.lock();
        let new_completion = *completion_guard + task.estimated_processing;
        *completion_guard = new_completion;
    }

    /// Pop the task with least slack time from the worker queue (non-blocking).
    ///
    /// # Returns
    /// `Some(task)` if a task was popped, `None` if the queue is empty
    fn try_pop(&self) -> Option<QueuedTask> {
        let mut guard = self.heap.lock();
        guard.pop() // Pop task with least slack time (most urgent)
    }

    /// Get the current estimated completion time for this worker.
    ///
    /// Used by the dispatcher to make optimal task assignments.
    ///
    /// # Returns
    /// Estimated time when this worker will finish processing all queued tasks
    fn estimated_completion_time(&self) -> Instant {
        *self.estimated_completion.lock()
    }

    /// Get the current queue depth (number of tasks waiting).
    ///
    /// Used by the dispatcher for load balancing.
    ///
    /// # Returns
    /// Number of tasks currently in the queue
    fn depth(&self) -> usize {
        self.heap.lock().len()
    }
}

/// Calculate slack time for a packet.
///
/// Slack time = deadline - current_time - estimated_processing_time
///
/// Negative slack means the task will likely miss its deadline.
/// Smaller slack means the task is more urgent and should be prioritized.
///
/// # Arguments
/// * `packet` - The packet to calculate slack for
/// * `now` - Current time
///
/// # Returns
/// Slack time as a Duration (may be negative if deadline will be missed)
fn calculate_slack_time(packet: &Packet, now: Instant) -> Duration {
    // Calculate absolute deadline: ingress timestamp + latency budget
    let deadline = packet.timestamp + packet.latency_budget;

    // Estimate processing time based on packet size
    let estimated_processing = processing_duration(packet);

    // Calculate slack time: deadline - current_time - estimated_processing
    // If slack is negative, the task will likely miss its deadline
    deadline.saturating_duration_since(now + estimated_processing)
}

/// Find the best worker for a task based on slack time and load balancing.
///
/// The clairvoyant dispatcher uses this heuristic to assign tasks optimally:
/// 1. Prefer workers with earlier estimated completion times (less loaded)
/// 2. Among workers with similar completion times, prefer the one that minimizes deadline misses
/// 3. Consider the task's slack time when making the assignment
///
/// # Arguments
/// * `task` - The task to assign
/// * `worker_queues` - All worker queues (one per worker)
/// * `now` - Current time
///
/// # Returns
/// Index of the best worker for this task
fn find_best_worker(task: &QueuedTask, worker_queues: &[Arc<WorkerQueue>], _now: Instant) -> usize {
    if worker_queues.is_empty() {
        return 0;
    }

    // Calculate when this task would complete on each worker
    let mut best_worker = 0;
    let mut best_score = i64::MIN;

    for (idx, queue) in worker_queues.iter().enumerate() {
        // Get estimated completion time for this worker
        let worker_completion = queue.estimated_completion_time();

        // Calculate when this task would complete on this worker
        let task_completion = worker_completion + task.estimated_processing;

        // Calculate remaining slack after assignment
        let remaining_slack = task.deadline.saturating_duration_since(task_completion);

        // Score: prioritize workers that:
        // 1. Have earlier completion times (less loaded)
        // 2. Result in positive remaining slack (task won't miss deadline)
        // 3. Have smaller queue depths (better load balancing)
        let queue_depth = queue.depth();
        let slack_ms = remaining_slack.as_millis() as i64;
        let depth_penalty = queue_depth as i64 * 1000; // Penalize deeper queues

        // Score = slack_time - depth_penalty (higher is better)
        // We want positive slack and shallow queues
        let score = slack_ms - depth_penalty;

        if score > best_score {
            best_score = score;
            best_worker = idx;
        }
    }

    best_worker
}

/// Structure to track P100 (maximum) latency for MEDIUM priority.
/// Uses a sliding window of recent latency measurements to track the maximum observed latency.
struct MediumLatencyTracker {
    /// Maximum observed latency (in microseconds)
    /// Protected by mutex for thread-safe updates
    max_latency_us: Arc<Mutex<u64>>,
    /// History of recent latency measurements for tracking maximum
    /// Protected by mutex for thread-safe updates
    latencies: Arc<Mutex<VecDeque<u64>>>,
    /// Maximum number of measurements to keep (sliding window size)
    window_size: usize,
}

impl MediumLatencyTracker {
    fn new() -> Self {
        Self {
            max_latency_us: Arc::new(Mutex::new(0)),
            latencies: Arc::new(Mutex::new(VecDeque::with_capacity(1000))),
            window_size: 1000, // Keep last 1000 measurements for P100 calculation
        }
    }

    /// Add a new latency measurement and update the maximum.
    fn record_latency(&self, latency: Duration) {
        let latency_us = latency.as_micros() as u64;
        let mut latencies = self.latencies.lock();
        let mut max_latency = self.max_latency_us.lock();

        latencies.push_back(latency_us);

        // Update maximum if this is higher
        if latency_us > *max_latency {
            *max_latency = latency_us;
        }

        // Keep only the last window_size measurements
        while latencies.len() > self.window_size {
            let removed = latencies.pop_front().unwrap();

            // If we removed the maximum, recalculate from remaining measurements
            if removed == *max_latency {
                *max_latency = latencies.iter().copied().max().unwrap_or(0);
            }
        }
    }

    /// Get the P100 (maximum observed) latency.
    /// Returns Duration::ZERO if no measurements are available.
    fn get_p100(&self) -> Duration {
        let max_latency = self.max_latency_us.lock();

        if *max_latency == 0 {
            Duration::ZERO
        } else {
            Duration::from_micros(*max_latency)
        }
    }
}

/// Structure to track P100 (maximum) latency for LOW priority.
/// Uses a sliding window of recent latency measurements to track the maximum observed latency.
struct LowLatencyTracker {
    /// Maximum observed latency (in microseconds)
    /// Protected by mutex for thread-safe updates
    max_latency_us: Arc<Mutex<u64>>,
    /// History of recent latency measurements for tracking maximum
    /// Protected by mutex for thread-safe updates
    latencies: Arc<Mutex<VecDeque<u64>>>,
    /// Maximum number of measurements to keep (sliding window size)
    window_size: usize,
}

impl LowLatencyTracker {
    fn new() -> Self {
        Self {
            max_latency_us: Arc::new(Mutex::new(0)),
            latencies: Arc::new(Mutex::new(VecDeque::with_capacity(1000))),
            window_size: 1000, // Keep last 1000 measurements for P100 calculation
        }
    }

    /// Add a new latency measurement and update the maximum.
    fn record_latency(&self, latency: Duration) {
        let latency_us = latency.as_micros() as u64;
        let mut latencies = self.latencies.lock();
        let mut max_latency = self.max_latency_us.lock();

        latencies.push_back(latency_us);

        // Update maximum if this is higher
        if latency_us > *max_latency {
            *max_latency = latency_us;
        }

        // Keep only the last window_size measurements
        while latencies.len() > self.window_size {
            let removed = latencies.pop_front().unwrap();

            // If we removed the maximum, recalculate from remaining measurements
            if removed == *max_latency {
                *max_latency = latencies.iter().copied().max().unwrap_or(0);
            }
        }
    }

    /// Get the P100 (maximum observed) latency.
    /// Returns Duration::ZERO if no measurements are available.
    fn get_p100(&self) -> Duration {
        let max_latency = self.max_latency_us.lock();

        if *max_latency == 0 {
            Duration::ZERO
        } else {
            Duration::from_micros(*max_latency)
        }
    }
}

pub struct CEDFScheduler {
    input_queues: PriorityTable<Arc<Receiver<Packet>>>,
    output_queues: PriorityTable<Sender<Packet>>,
    drop_counters: PriorityTable<Arc<AtomicU64>>,
    /// Worker queues stored in Arc<Mutex<>> to allow access from dispatcher thread
    worker_queues: Arc<Mutex<Vec<Arc<WorkerQueue>>>>,
    /// Tracks P100 (maximum) latency for MEDIUM priority to enable elastic guard windows
    medium_latency_tracker: MediumLatencyTracker,
    /// Tracks P100 (maximum) latency for LOW priority to enable elastic guard windows
    low_latency_tracker: LowLatencyTracker,
}

impl CEDFScheduler {
    /// Create a new CEDF scheduler.
    ///
    /// # Arguments
    /// * `input_queues` - Per-priority input channels from ingress DRR
    /// * `output_queues` - Per-priority output channels to egress DRR
    pub fn new(
        input_queues: PriorityTable<Arc<Receiver<Packet>>>,
        output_queues: PriorityTable<Sender<Packet>>,
    ) -> Self {
        let drop_counters = PriorityTable::from_fn(|_| Arc::new(AtomicU64::new(0)));
        // Create worker queues (will be populated when spawn_threads is called)
        // For now, create an empty vector (will be populated with actual worker count)
        let worker_queues = Arc::new(Mutex::new(Vec::new()));
        // Initialize MEDIUM P100 latency tracker for elastic guard windows
        let medium_latency_tracker = MediumLatencyTracker::new();
        // Initialize LOW P100 latency tracker for elastic guard windows
        let low_latency_tracker = LowLatencyTracker::new();
        Self {
            input_queues,
            output_queues,
            drop_counters,
            worker_queues,
            medium_latency_tracker,
            low_latency_tracker,
        }
    }

    /// Return drop counters for metrics emission.
    ///
    /// # Returns
    /// EDFDropCounters with per-priority drop counts
    pub fn get_drop_counts(&self) -> EDFDropCounters {
        EDFDropCounters {
            heap: 0, // CEDF doesn't have a shared heap, so no heap drops
            output: PriorityTable::from_fn(|priority| {
                self.drop_counters[priority].load(AtomicOrdering::Relaxed)
            }),
        }
    }

    /// Spawn dispatcher and worker threads.
    ///
    /// # Arguments
    /// * `running` - Atomic flag to signal shutdown
    /// * `set_priority` - Function to set thread priority
    /// * `set_core` - Function to pin thread to a core
    /// * `dispatcher_core` - Core ID for the dispatcher thread
    /// * `worker_cores` - Core IDs for worker threads
    pub fn spawn_threads(
        &self,
        running: Arc<AtomicBool>,
        set_priority: fn(i32),
        set_core: fn(usize),
        dispatcher_core: usize,
        worker_cores: &[usize],
    ) {
        // Create worker queues (one per worker)
        let worker_core_list: Vec<usize> = if worker_cores.is_empty() {
            vec![dispatcher_core]
        } else {
            worker_cores.to_vec()
        };

        let worker_queues: Vec<Arc<WorkerQueue>> = (0..worker_core_list.len())
            .map(|_| Arc::new(WorkerQueue::new()))
            .collect();

        // Store worker queues in the scheduler (for dispatcher access)
        {
            let mut queues_guard = self.worker_queues.lock();
            *queues_guard = worker_queues.clone();
        }

        let dispatcher_queues = self.worker_queues.clone();
        let dispatcher_inputs = self.input_queues.clone();
        let dispatcher_running = running.clone();

        // Spawn dispatcher thread
        thread::Builder::new()
            .name("CEDF-Dispatcher".to_string())
            .spawn(move || {
                set_priority(3);
                set_core(dispatcher_core);
                dispatcher_loop(dispatcher_inputs, dispatcher_queues, dispatcher_running);
            })
            .expect("failed to spawn CEDF dispatcher");

        // Spawn worker threads
        for (idx, &core_id) in worker_core_list.iter().enumerate() {
            let worker_queue = worker_queues[idx].clone();
            let worker_outputs = self.output_queues.clone();
            let worker_drops = self.drop_counters.clone();
            let worker_high_input = self.input_queues[Priority::High].clone();
            let worker_medium_input = self.input_queues[Priority::Medium].clone();
            let worker_medium_tracker = MediumLatencyTracker {
                max_latency_us: self.medium_latency_tracker.max_latency_us.clone(),
                latencies: self.medium_latency_tracker.latencies.clone(),
                window_size: self.medium_latency_tracker.window_size,
            };
            let worker_low_tracker = LowLatencyTracker {
                max_latency_us: self.low_latency_tracker.max_latency_us.clone(),
                latencies: self.low_latency_tracker.latencies.clone(),
                window_size: self.low_latency_tracker.window_size,
            };
            let worker_running = running.clone();
            thread::Builder::new()
                .name(format!("CEDF-Worker-{idx}"))
                .spawn(move || {
                    set_priority(3);
                    set_core(core_id);
                    worker_loop(
                        worker_queue,
                        worker_high_input,
                        worker_medium_input,
                        worker_medium_tracker,
                        worker_low_tracker,
                        worker_outputs,
                        worker_drops,
                        worker_running,
                    );
                })
                .expect("failed to spawn CEDF worker thread");
        }
    }
}

/// Dispatcher loop: reads packets from input queues and assigns them optimally to workers.
///
/// The clairvoyant dispatcher:
/// 1. Reads packets from all priority queues (non-blocking)
/// 2. Calculates slack time for each packet (deadline - current_time - estimated_processing)
/// 3. Assigns tasks to workers using an optimal heuristic (minimizes deadline misses, balances load)
/// 4. Updates worker estimated completion times
///
/// # Arguments
/// * `input_queues` - Per-priority input channels from ingress DRR
/// * `worker_queues` - Worker queues (one per worker)
/// * `running` - Atomic flag to signal shutdown
fn dispatcher_loop(
    input_queues: PriorityTable<Arc<Receiver<Packet>>>, // Input channels per priority
    worker_queues: Arc<Mutex<Vec<Arc<WorkerQueue>>>>,   // Worker queues (one per worker)
    running: Arc<AtomicBool>,                           // Shutdown flag (checked each iteration)
) {
    // Main dispatcher loop: continues until shutdown signal
    while running.load(AtomicOrdering::Relaxed) {
        let now = Instant::now();
        let mut dispatched = false;

        // AGGRESSIVE PRIORITIZATION: Process ALL HIGH priority packets first before any other priority
        // This ensures HIGH packets are dispatched immediately and not delayed by lower priorities
        loop {
            match input_queues[Priority::High].try_recv() {
                Ok(packet) => {
                    // HIGH priority packet: process immediately
                    let estimated_processing = processing_duration(&packet);
                    let deadline = packet.timestamp + packet.latency_budget;
                    let slack_time = calculate_slack_time(&packet, now);

                    let task = QueuedTask {
                        slack_time,
                        deadline,
                        priority: Priority::High,
                        packet,
                        estimated_processing,
                    };

                    let queues_guard = worker_queues.lock();
                    let best_worker_idx = find_best_worker(&task, &queues_guard, now);
                    let best_queue = queues_guard[best_worker_idx].clone();
                    drop(queues_guard);

                    best_queue.push(task);
                    dispatched = true;
                    // Continue processing HIGH packets until queue is empty
                }
                Err(TryRecvError::Empty) => {
                    // No more HIGH packets: break and process other priorities
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    // Channel closed: break
                    break;
                }
            }
        }

        // After processing all HIGH packets, process other priorities in order
        for priority in [Priority::Medium, Priority::Low, Priority::BestEffort] {
            // Try to receive a packet from this priority's input queue (non-blocking)
            match input_queues[priority].try_recv() {
                Ok(packet) => {
                    // Successfully received a packet: calculate slack time and assign to best worker

                    // Calculate slack time: deadline - current_time - estimated_processing
                    let estimated_processing = processing_duration(&packet);
                    let deadline = packet.timestamp + packet.latency_budget;
                    let slack_time = calculate_slack_time(&packet, now);

                    // Create task with slack time information
                    let task = QueuedTask {
                        slack_time,           // Used for worker queue ordering (least slack first)
                        deadline,             // Absolute deadline (for slack calculation)
                        priority,             // Priority class (for routing to output queue)
                        packet,               // The packet to process
                        estimated_processing, // Estimated processing time (for completion time updates)
                    };

                    // Find the best worker for this task using clairvoyant heuristic
                    let queues_guard = worker_queues.lock();
                    let best_worker_idx = find_best_worker(&task, &queues_guard, now);
                    let best_queue = queues_guard[best_worker_idx].clone();
                    drop(queues_guard); // Release lock before pushing

                    // Assign task to the best worker
                    best_queue.push(task);

                    // Mark that we dispatched at least one packet
                    dispatched = true;
                    // Only process one packet per lower priority per iteration
                    // This ensures HIGH packets get priority in the next iteration
                    break;
                }
                Err(TryRecvError::Empty) => {
                    // Queue empty for this priority: continue to next priority
                    continue;
                }
                Err(TryRecvError::Disconnected) => {
                    // Channel disconnected: continue to next priority
                    continue;
                }
            }
        }

        // OPTIMIZATION: Yield if no packets were dispatched (all queues were empty).
        // This prevents busy-waiting when there's no work to do.
        if !dispatched {
            thread::yield_now(); // Yield CPU to other threads
        }
    }
}

/// Worker loop: pops tasks from worker queue, processes them, forwards to output queues.
///
/// Each worker:
/// 1. Pops the task with least slack time from its queue (non-blocking, processes if available)
/// 2. For MEDIUM priority: applies elastic guard window to wait for HIGH priority packets
///    (active only if average latency is below maximum allowed latency)
/// 3. For LOW priority: applies elastic guard window to wait for MEDIUM priority packets
///    (active only if MEDIUM average latency is below maximum allowed latency)
/// 4. Processes the packet (simulated CPU work, size-dependent)
/// 5. Updates estimated completion time and latency tracking (for MEDIUM)
/// 6. Forwards the packet to the appropriate output queue (or drops if full)
///
/// Workers process tasks independently, with the dispatcher handling optimal assignment.
///
/// # Arguments
/// * `worker_queue` - This worker's queue (min-heap on slack time)
/// * `high_input_queue` - High priority input queue (for guard window checks on MEDIUM)
/// * `medium_input_queue` - Medium priority input queue (for guard window checks on LOW)
/// * `medium_latency_tracker` - Shared tracker for MEDIUM P100 latency
/// * `low_latency_tracker` - Shared tracker for LOW P100 latency
/// * `output_queues` - Per-priority output channels to egress DRR
/// * `drop_counters` - Per-priority atomic drop counters (for metrics)
/// * `running` - Atomic flag to signal shutdown
fn worker_loop(
    worker_queue: Arc<WorkerQueue>, // This worker's queue (min-heap on slack time)
    high_input_queue: Arc<Receiver<Packet>>, // High priority input queue (for MEDIUM guard windows)
    medium_input_queue: Arc<Receiver<Packet>>, // Medium priority input queue (for LOW guard windows)
    medium_latency_tracker: MediumLatencyTracker, // MEDIUM P100 latency tracker
    low_latency_tracker: LowLatencyTracker,    // LOW P100 latency tracker
    output_queues: PriorityTable<Sender<Packet>>, // Output channels per priority
    drop_counters: PriorityTable<Arc<AtomicU64>>, // Drop counters per priority
    running: Arc<AtomicBool>,                  // Shutdown flag
) {
    // Main worker loop: continues until shutdown signal
    while running.load(AtomicOrdering::Relaxed) {
        // Try to pop a task from this worker's queue (non-blocking)
        if let Some(mut task_to_process) = worker_queue.try_pop() {
            let priority = task_to_process.priority;
            let packet = task_to_process.packet.clone();
            let now = Instant::now();
            let mut final_priority = priority;

            // ========================================================================
            // STEP 1: Elastic guard window for MEDIUM priority tasks
            // ========================================================================
            // MEDIUM priority can wait for HIGH priority packets if there's space between
            // P100 latency and maximum allowed latency (10ms).
            // Guard window is active only if: p100_latency_medium < max_allowed_latency_medium
            if priority == Priority::Medium {
                // Get current P100 (maximum) latency for MEDIUM
                let p100_latency = medium_latency_tracker.get_p100();

                // Maximum allowed latency for MEDIUM is 10ms (latency_budget)
                let max_allowed_latency = packet.latency_budget; // Should be 10ms for MEDIUM

                // Check if there's space: p100_latency < max_allowed_latency
                // Also check if we have measurements for meaningful P100
                if p100_latency > Duration::ZERO && p100_latency < max_allowed_latency {
                    // Calculate available space for guard window
                    let space_available = max_allowed_latency.saturating_sub(p100_latency);

                    // Calculate elapsed time since packet ingress
                    let elapsed = now.saturating_duration_since(packet.timestamp);
                    let remaining_budget = packet.latency_budget.saturating_sub(elapsed);
                    let estimated_processing = processing_duration(&packet);

                    // Safety margin: reserve enough time to process the packet
                    // Use smaller safety margin (2x) to allow HIGH to benefit more from MEDIUM's budget
                    let safety_margin = estimated_processing.mul_f64(2.0);

                    // Guard threshold: use minimum of (space_available, remaining_budget - safety_margin)
                    // Cap at a larger maximum (e.g., 3ms) to give HIGH more opportunities
                    // HIGH should benefit from MEDIUM's larger budget (10ms) - be more aggressive
                    let guard_threshold = space_available
                        .min(remaining_budget.saturating_sub(safety_margin))
                        .min(Duration::from_millis(3));

                    // Only guard if we have meaningful threshold and sufficient budget
                    if guard_threshold.as_micros() > 0 && remaining_budget > safety_margin {
                        // Use smaller guard slices (50µs) for faster HIGH preemption response
                        let guard_slice = Duration::from_micros(50); // 50µs slices for faster checks
                        let guard_start = Instant::now();

                        // Guard loop: poll for High priority packets
                        'guard: loop {
                            let guard_elapsed =
                                Instant::now().saturating_duration_since(guard_start);
                            if guard_elapsed >= guard_threshold {
                                break; // Threshold reached, proceed with MEDIUM task
                            }

                            let remaining_guard = guard_threshold.saturating_sub(guard_elapsed);
                            if remaining_guard.is_zero() {
                                break;
                            }

                            let guard_window = remaining_guard.min(guard_slice);
                            let guard_deadline = Instant::now() + guard_window;

                            // Poll for High priority packets during this slice
                            while Instant::now() < guard_deadline {
                                match high_input_queue.try_recv() {
                                    Ok(high_packet) => {
                                        // High priority packet arrived! Preempt the MEDIUM task.
                                        let high_now = Instant::now();
                                        let high_deadline =
                                            high_packet.timestamp + high_packet.latency_budget;
                                        let high_estimated_processing =
                                            processing_duration(&high_packet);
                                        let high_slack_time = high_deadline
                                            .saturating_duration_since(
                                                high_now + high_estimated_processing,
                                            );

                                        // Re-queue the MEDIUM task back to worker queue
                                        worker_queue.push(task_to_process);

                                        // Create new task for High priority packet
                                        task_to_process = QueuedTask {
                                            slack_time: high_slack_time,
                                            deadline: high_deadline,
                                            priority: Priority::High,
                                            packet: high_packet,
                                            estimated_processing: high_estimated_processing,
                                        };
                                        final_priority = Priority::High;

                                        // Exit guard loop: we found a High packet to process
                                        break 'guard;
                                    }
                                    Err(TryRecvError::Empty) => {
                                        // No High packets available: spin briefly
                                        std::hint::spin_loop();
                                    }
                                    Err(TryRecvError::Disconnected) => {
                                        // Channel closed: exit guard loop
                                        break 'guard;
                                    }
                                }
                            }

                            // Re-check budget: exit if we're running out of time
                            let current_now = Instant::now();
                            let current_elapsed =
                                current_now.saturating_duration_since(packet.timestamp);
                            let current_remaining =
                                packet.latency_budget.saturating_sub(current_elapsed);

                            if current_remaining <= safety_margin {
                                break; // Not enough budget remaining, proceed with MEDIUM task
                            }
                        }
                    }
                }
            }

            // ========================================================================
            // STEP 1.5: Elastic guard window for LOW priority tasks
            // ========================================================================
            // LOW priority can wait for MEDIUM priority packets if LOW has space between
            // P100 latency and maximum allowed latency (100ms).
            // Guard window is active only if: p100_latency_low < max_allowed_latency_low
            if priority == Priority::Low {
                // Get current P100 (maximum) latency for LOW
                let p100_latency_low = low_latency_tracker.get_p100();

                // Maximum allowed latency for LOW is 100ms (latency_budget)
                let max_allowed_latency_low = packet.latency_budget; // Should be 100ms for LOW

                // Check if LOW has space: p100_latency_low < max_allowed_latency_low
                // Also check if we have measurements for meaningful P100
                if p100_latency_low > Duration::ZERO && p100_latency_low < max_allowed_latency_low {
                    // Calculate available space for LOW (this determines if LOW can wait)
                    let low_space_available =
                        max_allowed_latency_low.saturating_sub(p100_latency_low);

                    // Calculate elapsed time since packet ingress
                    let elapsed = now.saturating_duration_since(packet.timestamp);
                    let remaining_budget = packet.latency_budget.saturating_sub(elapsed);
                    let estimated_processing = processing_duration(&packet);

                    // Safety margin: reserve enough time to process the packet
                    // Use smaller safety margin (2x) to allow HIGH/MEDIUM to benefit more from LOW's budget
                    let safety_margin = estimated_processing.mul_f64(2.0);

                    // Guard threshold: use minimum of (low_space_available, remaining_budget - safety_margin)
                    // Cap at a larger maximum (e.g., 10ms) to give HIGH/MEDIUM many more opportunities
                    // LOW has 100ms budget, so it can wait much longer for HIGH/MEDIUM
                    // Be more aggressive to help HIGH converge to its 1ms target
                    let guard_threshold = low_space_available
                        .min(remaining_budget.saturating_sub(safety_margin))
                        .min(Duration::from_millis(10));

                    // Only guard if we have meaningful threshold and sufficient budget
                    if guard_threshold.as_micros() > 0 && remaining_budget > safety_margin {
                        // Use smaller guard slices (50µs) for faster HIGH/MEDIUM preemption response
                        let guard_slice = Duration::from_micros(50); // 50µs slices for faster checks
                        let guard_start = Instant::now();

                        // Guard loop: poll for HIGH and Medium priority packets
                        // HIGH can preempt LOW during guard window (highest priority)
                        'guard_low: loop {
                            let guard_elapsed =
                                Instant::now().saturating_duration_since(guard_start);
                            if guard_elapsed >= guard_threshold {
                                break; // Threshold reached, proceed with LOW task
                            }

                            let remaining_guard = guard_threshold.saturating_sub(guard_elapsed);
                            if remaining_guard.is_zero() {
                                break;
                            }

                            let guard_window = remaining_guard.min(guard_slice);
                            let guard_deadline = Instant::now() + guard_window;

                            // Poll for HIGH and Medium priority packets during this slice
                            // HIGH has the highest priority, so check it first
                            while Instant::now() < guard_deadline {
                                // First, always check for High priority packets (highest priority)
                                match high_input_queue.try_recv() {
                                    Ok(high_packet) => {
                                        // High priority packet arrived! Preempt the LOW task immediately.
                                        let high_now = Instant::now();
                                        let high_deadline =
                                            high_packet.timestamp + high_packet.latency_budget;
                                        let high_estimated_processing =
                                            processing_duration(&high_packet);
                                        let high_slack_time = high_deadline
                                            .saturating_duration_since(
                                                high_now + high_estimated_processing,
                                            );

                                        // Re-queue the LOW task back to worker queue
                                        worker_queue.push(task_to_process);

                                        // Create new task for High priority packet
                                        task_to_process = QueuedTask {
                                            slack_time: high_slack_time,
                                            deadline: high_deadline,
                                            priority: Priority::High,
                                            packet: high_packet,
                                            estimated_processing: high_estimated_processing,
                                        };
                                        final_priority = Priority::High;

                                        // Exit guard loop: we found a High packet to process
                                        break 'guard_low;
                                    }
                                    Err(TryRecvError::Empty) => {
                                        // No High packets available: continue to check Medium
                                    }
                                    Err(TryRecvError::Disconnected) => {
                                        // Channel closed: continue to check Medium
                                    }
                                }

                                // Then check for Medium priority packets
                                match medium_input_queue.try_recv() {
                                    Ok(medium_packet) => {
                                        // Medium priority packet arrived! Preempt the LOW task.
                                        let medium_now = Instant::now();
                                        let medium_deadline =
                                            medium_packet.timestamp + medium_packet.latency_budget;
                                        let medium_estimated_processing =
                                            processing_duration(&medium_packet);
                                        let medium_slack_time = medium_deadline
                                            .saturating_duration_since(
                                                medium_now + medium_estimated_processing,
                                            );

                                        // Re-queue the LOW task back to worker queue
                                        worker_queue.push(task_to_process);

                                        // Create new task for Medium priority packet
                                        task_to_process = QueuedTask {
                                            slack_time: medium_slack_time,
                                            deadline: medium_deadline,
                                            priority: Priority::Medium,
                                            packet: medium_packet,
                                            estimated_processing: medium_estimated_processing,
                                        };
                                        final_priority = Priority::Medium;

                                        // Exit guard loop: we found a Medium packet to process
                                        break 'guard_low;
                                    }
                                    Err(TryRecvError::Empty) => {
                                        // No Medium packets available: spin briefly
                                        std::hint::spin_loop();
                                    }
                                    Err(TryRecvError::Disconnected) => {
                                        // Channel closed: continue (we can still process High)
                                    }
                                }
                            }

                            // Re-check budget: exit if we're running out of time
                            let current_now = Instant::now();
                            let current_elapsed =
                                current_now.saturating_duration_since(packet.timestamp);
                            let current_remaining =
                                packet.latency_budget.saturating_sub(current_elapsed);

                            if current_remaining <= safety_margin {
                                break; // Not enough budget remaining, proceed with LOW task
                            }
                        }
                    }
                }
            }

            // ========================================================================
            // STEP 2: Simulate packet processing (busy-wait with preemption checks)
            // ========================================================================
            // Calculate processing time based on packet size (0.1-0.3 ms)
            let mut processing_time = task_to_process.estimated_processing;
            let mut start = Instant::now();
            let original_packet = task_to_process.packet.clone();
            let original_priority = task_to_process.priority;

            // Busy-wait loop: spin until processing time has elapsed
            // For MEDIUM/LOW priorities: check periodically for HIGH/MEDIUM packets
            // This allows HIGH to benefit from the larger latency budgets of MEDIUM/LOW
            // IMPORTANT: HIGH priority packets can NEVER be preempted - they must complete once started
            // Check very frequently (every 10µs) to allow HIGH to preempt MEDIUM/LOW as quickly as possible
            // This helps HIGH converge to its 1ms target by being very responsive
            let check_interval = Duration::from_micros(10); // Check every 10µs for preemption
            let mut last_check = start;

            while start.elapsed() < processing_time {
                let now = Instant::now();

                // For MEDIUM and LOW priorities only: check for higher priority packets periodically
                // during processing if they still have budget remaining
                // HIGH priority packets are NEVER preempted - they complete once processing starts
                if original_priority != Priority::High
                    && (original_priority == Priority::Medium || original_priority == Priority::Low)
                    && now.saturating_duration_since(last_check) >= check_interval
                {
                    last_check = now;

                    // Check elapsed time since packet ingress
                    let elapsed_since_ingress =
                        now.saturating_duration_since(original_packet.timestamp);
                    let remaining_budget = original_packet
                        .latency_budget
                        .saturating_sub(elapsed_since_ingress);
                    let estimated_remaining = processing_time.saturating_sub(start.elapsed());
                    // Use minimal safety margin (1.2x) to allow preemption even with very little budget
                    // This allows HIGH to benefit much more from MEDIUM/LOW's budgets
                    // Be very aggressive to help HIGH converge to its 1ms target
                    let safety_margin = estimated_remaining.mul_f64(1.2);

                    // Check for preemption if we have budget remaining (even if it's very tight)
                    // HIGH should be able to preempt MEDIUM/LOW very aggressively
                    if remaining_budget > safety_margin {
                        // For MEDIUM: check for HIGH priority packets
                        if original_priority == Priority::Medium {
                            if let Ok(high_packet) = high_input_queue.try_recv() {
                                // HIGH packet arrived during processing! Preempt immediately.
                                let high_now = Instant::now();
                                let high_deadline =
                                    high_packet.timestamp + high_packet.latency_budget;
                                let high_estimated_processing = processing_duration(&high_packet);
                                let high_slack_time = high_deadline.saturating_duration_since(
                                    high_now + high_estimated_processing,
                                );

                                // Re-calculate elapsed processing time for the original MEDIUM task
                                let elapsed_processing = start.elapsed();
                                let remaining_processing =
                                    processing_time.saturating_sub(elapsed_processing);

                                // Update the original task's estimated processing (remaining time)
                                task_to_process.estimated_processing = remaining_processing;

                                // Re-queue the MEDIUM task back to worker queue
                                worker_queue.push(task_to_process);

                                // Create new task for High priority packet
                                task_to_process = QueuedTask {
                                    slack_time: high_slack_time,
                                    deadline: high_deadline,
                                    priority: Priority::High,
                                    packet: high_packet,
                                    estimated_processing: high_estimated_processing,
                                };
                                final_priority = Priority::High;

                                // Restart processing with HIGH packet
                                start = Instant::now();
                                processing_time = task_to_process.estimated_processing;
                                continue; // Process HIGH packet instead
                            }
                        }

                        // For LOW: check for HIGH and MEDIUM priority packets
                        if original_priority == Priority::Low {
                            // First check for HIGH (highest priority)
                            if let Ok(high_packet) = high_input_queue.try_recv() {
                                // HIGH packet arrived during processing! Preempt immediately.
                                let high_now = Instant::now();
                                let high_deadline =
                                    high_packet.timestamp + high_packet.latency_budget;
                                let high_estimated_processing = processing_duration(&high_packet);
                                let high_slack_time = high_deadline.saturating_duration_since(
                                    high_now + high_estimated_processing,
                                );

                                // Re-calculate elapsed processing time for the original LOW task
                                let elapsed_processing = start.elapsed();
                                let remaining_processing =
                                    processing_time.saturating_sub(elapsed_processing);

                                // Update the original task's estimated processing (remaining time)
                                task_to_process.estimated_processing = remaining_processing;

                                // Re-queue the LOW task back to worker queue
                                worker_queue.push(task_to_process);

                                // Create new task for High priority packet
                                task_to_process = QueuedTask {
                                    slack_time: high_slack_time,
                                    deadline: high_deadline,
                                    priority: Priority::High,
                                    packet: high_packet,
                                    estimated_processing: high_estimated_processing,
                                };
                                final_priority = Priority::High;

                                // Restart processing with HIGH packet
                                start = Instant::now();
                                processing_time = task_to_process.estimated_processing;
                                continue; // Process HIGH packet instead
                            }

                            // Then check for MEDIUM
                            if let Ok(medium_packet) = medium_input_queue.try_recv() {
                                // MEDIUM packet arrived during processing! Preempt immediately.
                                let medium_now = Instant::now();
                                let medium_deadline =
                                    medium_packet.timestamp + medium_packet.latency_budget;
                                let medium_estimated_processing =
                                    processing_duration(&medium_packet);
                                let medium_slack_time = medium_deadline.saturating_duration_since(
                                    medium_now + medium_estimated_processing,
                                );

                                // Re-calculate elapsed processing time for the original LOW task
                                let elapsed_processing = start.elapsed();
                                let remaining_processing =
                                    processing_time.saturating_sub(elapsed_processing);

                                // Update the original task's estimated processing (remaining time)
                                task_to_process.estimated_processing = remaining_processing;

                                // Re-queue the LOW task back to worker queue
                                worker_queue.push(task_to_process);

                                // Create new task for Medium priority packet
                                task_to_process = QueuedTask {
                                    slack_time: medium_slack_time,
                                    deadline: medium_deadline,
                                    priority: Priority::Medium,
                                    packet: medium_packet,
                                    estimated_processing: medium_estimated_processing,
                                };
                                final_priority = Priority::Medium;

                                // Restart processing with MEDIUM packet
                                start = Instant::now();
                                processing_time = task_to_process.estimated_processing;
                                continue; // Process MEDIUM packet instead
                            }
                        }
                    }
                }

                std::hint::spin_loop(); // CPU hint: indicates tight spin loop
            }

            // ========================================================================
            // STEP 3: Update estimated completion time
            // ========================================================================
            // Task completed: update estimated completion time
            let completion_now = Instant::now();
            let mut completion_guard = worker_queue.estimated_completion.lock();
            // Update completion time to now (we just finished processing)
            *completion_guard = completion_now;

            // ========================================================================
            // STEP 3.5: Update MEDIUM and LOW P100 latency if this was a MEDIUM/LOW packet
            // ========================================================================
            // Track P100 (maximum) latency for MEDIUM and LOW priorities to enable elastic guard windows
            let observed_latency =
                completion_now.saturating_duration_since(task_to_process.packet.timestamp);

            // Update MEDIUM P100 latency if this was a MEDIUM packet
            if final_priority == Priority::Medium {
                medium_latency_tracker.record_latency(observed_latency);
            }

            // Update LOW P100 latency if this was a LOW packet
            if final_priority == Priority::Low {
                low_latency_tracker.record_latency(observed_latency);
            }

            // ========================================================================
            // STEP 4: Forward packet to output queue (or drop if full)
            // ========================================================================
            // Try to send to the egress DRR queue (non-blocking)
            if output_queues[final_priority]
                .try_send(task_to_process.packet)
                .is_err()
            {
                // Send failed (queue full): increment drop counter atomically
                // Relaxed ordering is sufficient: this is just a metric, not synchronization
                drop_counters[final_priority].fetch_add(1, AtomicOrdering::Relaxed);
            }
            // If send succeeded, packet is forwarded to egress DRR
        } else {
            // No tasks available: yield briefly to avoid busy-waiting
            thread::yield_now();
        }
    }
    // Loop exits when running flag becomes false (shutdown)
}
