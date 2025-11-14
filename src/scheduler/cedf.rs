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
fn find_best_worker(
    task: &QueuedTask,
    worker_queues: &[Arc<WorkerQueue>],
    _now: Instant,
) -> usize {
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

pub struct CEDFScheduler {
    input_queues: PriorityTable<Arc<Receiver<Packet>>>,
    output_queues: PriorityTable<Sender<Packet>>,
    drop_counters: PriorityTable<Arc<AtomicU64>>,
    /// Worker queues stored in Arc<Mutex<>> to allow access from dispatcher thread
    worker_queues: Arc<Mutex<Vec<Arc<WorkerQueue>>>>,
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
        Self {
            input_queues,
            output_queues,
            drop_counters,
            worker_queues,
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
                dispatcher_loop(
                    dispatcher_inputs,
                    dispatcher_queues,
                    dispatcher_running,
                );
            })
            .expect("failed to spawn CEDF dispatcher");

        // Spawn worker threads
        for (idx, &core_id) in worker_core_list.iter().enumerate() {
            let worker_queue = worker_queues[idx].clone();
            let worker_outputs = self.output_queues.clone();
            let worker_drops = self.drop_counters.clone();
            let worker_running = running.clone();
            thread::Builder::new()
                .name(format!("CEDF-Worker-{idx}"))
                .spawn(move || {
                    set_priority(3);
                    set_core(core_id);
                    worker_loop(
                        worker_queue,
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
    worker_queues: Arc<Mutex<Vec<Arc<WorkerQueue>>>>, // Worker queues (one per worker)
    running: Arc<AtomicBool>, // Shutdown flag (checked each iteration)
) {
    // Main dispatcher loop: continues until shutdown signal
    while running.load(AtomicOrdering::Relaxed) {
        let now = Instant::now();
        let mut dispatched = false;

        // Process packets from all priority queues in priority order
        for priority in [Priority::High, Priority::Medium, Priority::Low, Priority::BestEffort] {
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
                        slack_time, // Used for worker queue ordering (least slack first)
                        deadline, // Absolute deadline (for slack calculation)
                        priority, // Priority class (for routing to output queue)
                        packet, // The packet to process
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
/// 2. Processes the packet (simulated CPU work, size-dependent)
/// 3. Updates estimated completion time
/// 4. Forwards the packet to the appropriate output queue (or drops if full)
///
/// Workers process tasks independently, with the dispatcher handling optimal assignment.
///
/// # Arguments
/// * `worker_queue` - This worker's queue (min-heap on slack time)
/// * `output_queues` - Per-priority output channels to egress DRR
/// * `drop_counters` - Per-priority atomic drop counters (for metrics)
/// * `running` - Atomic flag to signal shutdown
fn worker_loop(
    worker_queue: Arc<WorkerQueue>, // This worker's queue (min-heap on slack time)
    output_queues: PriorityTable<Sender<Packet>>, // Output channels per priority
    drop_counters: PriorityTable<Arc<AtomicU64>>, // Drop counters per priority
    running: Arc<AtomicBool>, // Shutdown flag
) {
    // Main worker loop: continues until shutdown signal
    while running.load(AtomicOrdering::Relaxed) {
        // Try to pop a task from this worker's queue (non-blocking)
        if let Some(task) = worker_queue.try_pop() {
            // ========================================================================
            // STEP 1: Simulate packet processing (busy-wait)
            // ========================================================================
            // Calculate processing time based on packet size (0.1-0.3 ms)
            let processing_time = task.estimated_processing;
            let start = Instant::now();
            // Busy-wait loop: spin until processing time has elapsed
            while start.elapsed() < processing_time {
                std::hint::spin_loop(); // CPU hint: indicates tight spin loop
            }

            // ========================================================================
            // STEP 2: Update estimated completion time
            // ========================================================================
            // Task completed: update estimated completion time
            let completion_now = Instant::now();
            let mut completion_guard = worker_queue.estimated_completion.lock();
            // Update completion time to now (we just finished processing)
            *completion_guard = completion_now;

            // ========================================================================
            // STEP 3: Forward packet to output queue (or drop if full)
            // ========================================================================
            // Try to send to the egress DRR queue (non-blocking)
            if output_queues[task.priority].try_send(task.packet).is_err() {
                // Send failed (queue full): increment drop counter atomically
                // Relaxed ordering is sufficient: this is just a metric, not synchronization
                drop_counters[task.priority].fetch_add(1, AtomicOrdering::Relaxed);
            }
            // If send succeeded, packet is forwarded to egress DRR
        } else {
            // No tasks available: yield briefly to avoid busy-waiting
            thread::yield_now();
        }
    }
    // Loop exits when running flag becomes false (shutdown)
}

