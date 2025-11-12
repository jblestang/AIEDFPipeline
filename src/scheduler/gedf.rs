//! Global Earliest Deadline First (G-EDF) Scheduler with Shared Run Queue
//!
//! This scheduler implements a global EDF algorithm where all worker threads share a single
//! priority queue (min-heap on deadline). A dispatcher thread reads from input queues and
//! pushes tasks into the shared queue. Worker threads pop tasks from the shared queue and
//! process them.
//!
//! Algorithm:
//! 1. Dispatcher: reads packets from per-priority input queues, calculates deadlines, pushes to shared queue
//! 2. Workers: pop earliest-deadline tasks from shared queue, process them, forward to output queues
//! 3. Shared queue: protected by mutex + condition variable for efficient blocking/waking
//!
//! The shared queue ensures true global EDF: the earliest deadline across all priorities is always processed first.

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
// Import mutex and condition variable for shared queue synchronization
use parking_lot::{Condvar, Mutex};
// Import ordering trait for deadline comparisons
use std::cmp::Ordering;
// Import BinaryHeap for min-heap on deadline
use std::collections::BinaryHeap;
// Import atomic types for lock-free flags and counters
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
// Import Arc for shared ownership
use std::sync::Arc;
// Import thread utilities for spawning worker threads
use std::thread;
// Import time types for deadlines
use std::time::Instant;

/// Task queued in the shared run queue, ordered by deadline (earliest first).
///
/// Contains a packet, its priority, and its absolute deadline. The shared queue
/// is a min-heap on deadline, so the earliest-deadline task is always at the top.
#[derive(Debug)]
struct QueuedTask {
    /// Absolute deadline: timestamp + latency_budget (when the packet must be completed)
    deadline: Instant,
    /// Priority class of the packet (for routing to correct output queue)
    priority: Priority,
    /// The packet to process
    packet: Packet,
}

impl Ord for QueuedTask {
    /// Compare by deadline in reverse order to create a min-heap.
    ///
    /// BinaryHeap is a max-heap by default, so we reverse the comparison:
    /// - `other.deadline.cmp(&self.deadline)` means earlier deadlines are "greater"
    /// - This makes the earliest-deadline task rise to the top of the heap
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse comparison so BinaryHeap pops the earliest deadline first.
        // Earlier deadlines are "greater" in this ordering (for min-heap behavior)
        other.deadline.cmp(&self.deadline)
    }
}

impl PartialOrd for QueuedTask {
    /// Delegates to `Ord::cmp` for total ordering.
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for QueuedTask {
    /// Two tasks are equal if they have the same deadline.
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline
    }
}

impl Eq for QueuedTask {}

/// Shared run queue protected by mutex and condition variable.
///
/// The queue is a min-heap on deadline. Workers block on the condition variable
/// when the queue is empty, and the dispatcher wakes them when new tasks arrive.
struct SharedQueue {
    /// Min-heap of tasks ordered by deadline (earliest first)
    /// Protected by mutex to allow concurrent push/pop operations
    heap: Mutex<BinaryHeap<QueuedTask>>,
    /// Condition variable for efficient blocking/waking
    /// Workers wait on this when the queue is empty, dispatcher notifies when tasks arrive
    available: Condvar,
}

impl SharedQueue {
    /// Create a new shared queue with empty heap and condition variable.
    fn new() -> Self {
        Self {
            // Initialize empty heap (will be populated by dispatcher)
            heap: Mutex::new(BinaryHeap::new()),
            // Initialize condition variable (for worker blocking/waking)
            available: Condvar::new(),
        }
    }

    /// Push a task into the shared queue and wake one waiting worker.
    ///
    /// # Arguments
    /// * `task` - The task to enqueue (contains packet, priority, deadline)
    fn push(&self, task: QueuedTask) {
        {
            // Acquire mutex and push task into heap
            let mut guard = self.heap.lock();
            guard.push(task); // Push task (heap maintains min-deadline order)
            // Mutex is released here (RAII)
        }
        // Wake one waiting worker (if any) to process the new task
        self.available.notify_one();
    }

    /// Pop the earliest-deadline task from the shared queue, blocking if empty.
    ///
    /// Blocks on the condition variable if the queue is empty, and returns `None`
    /// if shutdown is requested while waiting.
    ///
    /// # Arguments
    /// * `running` - Atomic flag to signal shutdown
    ///
    /// # Returns
    /// `Some(task)` if a task was popped, `None` if shutdown was requested
    fn pop(&self, running: &AtomicBool) -> Option<QueuedTask> {
        // Acquire mutex to access the heap
        let mut guard = self.heap.lock();
        loop {
            // Try to pop a task from the heap
            if let Some(task) = guard.pop() {
                // Found a task: return it (mutex is released by RAII)
                return Some(task);
            }
            // Heap is empty: check if we should shutdown
            if !running.load(AtomicOrdering::Relaxed) {
                // Shutdown requested: return None
                return None;
            }
            // Heap is empty and we're still running: wait for new tasks
            // This releases the mutex and blocks until notified
            self.available.wait(&mut guard);
            // When we wake up, the mutex is re-acquired and we loop to check again
        }
    }

    /// Wake all waiting workers (used during shutdown).
    ///
    /// Called by the dispatcher when it exits to ensure all workers wake up and exit.
    fn wake_all(&self) {
        // Wake all workers waiting on the condition variable
        self.available.notify_all();
    }
}

pub struct GEDFScheduler {
    input_queues: PriorityTable<Arc<Receiver<Packet>>>,
    output_queues: PriorityTable<Sender<Packet>>,
    drop_counters: PriorityTable<Arc<AtomicU64>>,
    shared_queue: Arc<SharedQueue>,
}

impl GEDFScheduler {
    pub fn new(
        input_queues: PriorityTable<Arc<Receiver<Packet>>>,
        output_queues: PriorityTable<Sender<Packet>>,
    ) -> Self {
        let drop_counters = PriorityTable::from_fn(|_| Arc::new(AtomicU64::new(0)));
        Self {
            input_queues,
            output_queues,
            drop_counters,
            shared_queue: Arc::new(SharedQueue::new()),
        }
    }

    pub fn get_drop_counts(&self) -> EDFDropCounters {
        EDFDropCounters {
            heap: 0,
            output: PriorityTable::from_fn(|priority| {
                self.drop_counters[priority].load(AtomicOrdering::Relaxed)
            }),
        }
    }

    pub fn spawn_threads(
        &self,
        running: Arc<AtomicBool>,
        set_priority: fn(i32),
        set_core: fn(usize),
        dispatcher_core: usize,
        worker_cores: &[usize],
    ) {
        let dispatcher_running = running.clone();
        let dispatcher_queue = self.shared_queue.clone();
        let dispatcher_inputs = self.input_queues.clone();
        thread::Builder::new()
            .name("GEDF-Dispatcher".to_string())
            .spawn(move || {
                set_priority(3);
                set_core(dispatcher_core);
                dispatcher_loop(dispatcher_inputs, dispatcher_queue, dispatcher_running);
            })
            .expect("failed to spawn GEDF dispatcher");

        let worker_core_list: Vec<usize> = if worker_cores.is_empty() {
            vec![dispatcher_core]
        } else {
            worker_cores.to_vec()
        };

        for (idx, &core_id) in worker_core_list.iter().enumerate() {
            let worker_queue = self.shared_queue.clone();
            let worker_outputs = self.output_queues.clone();
            let worker_drops = self.drop_counters.clone();
            let worker_running = running.clone();
            thread::Builder::new()
                .name(format!("GEDF-Worker-{idx}"))
                .spawn(move || {
                    set_priority(3);
                    set_core(core_id);
                    worker_loop(worker_queue, worker_outputs, worker_drops, worker_running);
                })
                .expect("failed to spawn GEDF worker thread");
        }
    }
}

/// Dispatcher loop: reads packets from input queues and pushes them to the shared queue.
///
/// The dispatcher:
/// 1. Iterates through priorities in order (High → Medium → Low → BestEffort)
/// 2. Tries to receive a packet from each priority's input queue (non-blocking)
/// 3. Calculates the absolute deadline (timestamp + latency_budget)
/// 4. Pushes the task to the shared queue (wakes a waiting worker)
/// 5. Yields if no packets were dispatched (prevents busy-waiting)
///
/// # Arguments
/// * `input_queues` - Per-priority input channels from ingress DRR
/// * `shared_queue` - Shared run queue (min-heap on deadline)
/// * `running` - Atomic flag to signal shutdown
fn dispatcher_loop(
    input_queues: PriorityTable<Arc<Receiver<Packet>>>, // Input channels per priority
    shared_queue: Arc<SharedQueue>, // Shared run queue (min-heap on deadline)
    running: Arc<AtomicBool>, // Shutdown flag (checked each iteration)
) {
    // Main dispatcher loop: continues until shutdown signal
    while running.load(AtomicOrdering::Relaxed) {
        // Track if we dispatched any packets this iteration (for yield optimization)
        let mut dispatched = false;

        // Iterate through priorities in order (High, Medium, Low, BestEffort)
        for priority in Priority::ALL {
            // Try to receive a packet from this priority's input queue (non-blocking)
            match input_queues[priority].try_recv() {
                Ok(packet) => {
                    // Successfully received a packet: calculate deadline and enqueue
                    // Calculate absolute deadline: ingress timestamp + latency budget
                    let deadline = packet.timestamp + packet.latency_budget;
                    // Push task to shared queue (wakes a waiting worker if queue was empty)
                    shared_queue.push(QueuedTask {
                        deadline, // EDF deadline (for heap ordering)
                        priority, // Priority class (for routing to output queue)
                        packet, // The packet to process
                    });
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

    // Shutdown: wake all waiting workers so they can exit
    shared_queue.wake_all();
}

/// Worker loop: pops tasks from shared queue, processes them, forwards to output queues.
///
/// Each worker:
/// 1. Pops the earliest-deadline task from the shared queue (blocks if empty)
/// 2. Processes the packet (simulated CPU work, size-dependent)
/// 3. Forwards the packet to the appropriate output queue (or drops if full)
///
/// Multiple workers can run in parallel, all competing for tasks from the same shared queue.
/// This ensures true global EDF: the earliest deadline across all priorities is always processed first.
///
/// # Arguments
/// * `shared_queue` - Shared run queue (min-heap on deadline)
/// * `output_queues` - Per-priority output channels to egress DRR
/// * `drop_counters` - Per-priority atomic drop counters (for metrics)
/// * `running` - Atomic flag to signal shutdown
fn worker_loop(
    shared_queue: Arc<SharedQueue>, // Shared run queue (min-heap on deadline)
    output_queues: PriorityTable<Sender<Packet>>, // Output channels per priority
    drop_counters: PriorityTable<Arc<AtomicU64>>, // Drop counters per priority
    running: Arc<AtomicBool>, // Shutdown flag (passed to pop for early exit)
) {
    // Main worker loop: continues until shared_queue.pop returns None (shutdown)
    // The pop operation blocks if the queue is empty, and returns None on shutdown.
    while let Some(task) = shared_queue.pop(&running) {
        // Extract packet and priority from the task
        let packet = task.packet; // The packet to process
        let priority = task.priority; // Priority class (for routing)

        // ========================================================================
        // STEP 1: Simulate packet processing (busy-wait)
        // ========================================================================
        // Calculate processing time based on packet size (0.05-0.15 ms)
        let processing_time = processing_duration(&packet);
        let start = Instant::now();
        // Busy-wait loop: spin until processing time has elapsed
        while start.elapsed() < processing_time {
            std::hint::spin_loop(); // CPU hint: indicates tight spin loop
        }

        // ========================================================================
        // STEP 2: Forward packet to output queue (or drop if full)
        // ========================================================================
        // Try to send to the egress DRR queue (non-blocking)
        if output_queues[priority].try_send(packet).is_err() {
            // Send failed (queue full): increment drop counter atomically
            // Relaxed ordering is sufficient: this is just a metric, not synchronization
            drop_counters[priority].fetch_add(1, AtomicOrdering::Relaxed);
        }
        // If send succeeded, packet is forwarded to egress DRR
    }
    // Loop exits when shared_queue.pop returns None (shutdown or queue closed)
}
