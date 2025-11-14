//! Global EDF with Virtual Deadlines (G-EDF-VD) Scheduler
//!
//! Implements a shared run-queue scheduler where tasks compete based on virtual deadlines derived
//! from the original latency budget multiplied by a per-priority scaling factor. High priority
//! tasks get the smallest scaling factor (earliest virtual deadlines), strongly prioritizing them
//! in the scheduling queue.
//!
//! Algorithm:
//! 1. Dispatcher: reads packets, calculates virtual deadlines (original deadline × scaling factor), pushes to shared queue
//! 2. Workers: pop earliest-virtual-deadline tasks from shared queue, process them, forward to output queues
//! 3. Virtual deadlines: High=0.05 (earliest), Medium=0.6, Low=0.75, BestEffort=1.0 (latest)
//!
//! Virtual deadlines give HIGH priority tasks the earliest virtual deadlines, making them appear
//! more urgent in the scheduling queue. This strongly prioritizes high-priority tasks, ensuring
//! they are processed before lower-priority ones even when deadlines are similar.

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
// Import BinaryHeap for min-heap on virtual deadline
use std::collections::BinaryHeap;
// Import atomic types for lock-free flags and counters
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
// Import Arc for shared ownership
use std::sync::Arc;
// Import thread utilities for spawning worker threads
use std::thread;
// Import time types for deadlines
use std::time::Instant;

/// Task queued in the shared run queue, ordered by virtual deadline (earliest first).
///
/// Contains a packet, its priority, and its virtual deadline (scaled from the original deadline).
/// The shared queue is a min-heap on virtual deadline, so the earliest-virtual-deadline task
/// is always at the top.
#[derive(Debug)]
struct QueuedTask {
    /// Virtual deadline: timestamp + (latency_budget × scaling_factor)
    /// HIGH priority has the smallest scaling factor (0.5), making its virtual deadlines earliest
    /// (giving HIGH priority tasks a "head start" in the scheduling queue)
    virtual_deadline: Instant,
    /// Priority class of the packet (for routing to correct output queue)
    priority: Priority,
    /// The packet to process
    packet: Packet,
}

impl Ord for QueuedTask {
    fn cmp(&self, other: &Self) -> Ordering {
        other.virtual_deadline.cmp(&self.virtual_deadline)
    }
}

impl PartialOrd for QueuedTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for QueuedTask {
    fn eq(&self, other: &Self) -> bool {
        self.virtual_deadline == other.virtual_deadline
    }
}

impl Eq for QueuedTask {}

/// Shared run queue protected by mutex and condition variable.
///
/// The queue is a min-heap on virtual deadline. Workers block on the condition variable
/// when the queue is empty, and the dispatcher wakes them when new tasks arrive.
struct SharedQueue {
    /// Min-heap of tasks ordered by virtual deadline (earliest first)
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
    /// * `task` - The task to enqueue (contains packet, priority, virtual_deadline)
    fn push(&self, task: QueuedTask) {
        {
            // Acquire mutex and push task into heap
            let mut guard = self.heap.lock();
            guard.push(task); // Push task (heap maintains min-virtual-deadline order)
            // Mutex is released here (RAII)
        }
        // Wake one waiting worker (if any) to process the new task
        self.available.notify_one();
    }

    /// Pop the earliest-virtual-deadline task from the shared queue, blocking if empty.
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

/// Get the per-priority scaling factors for virtual deadlines.
///
/// Virtual deadlines are calculated as: `virtual_deadline = timestamp + (latency_budget × scaling_factor)`
///
/// Scaling factors (INVERTED to prioritize HIGH priority):
/// - High: 0.05 (95% reduction, virtual deadline is 5% of original - EARLIEST)
/// - Medium: 0.6 (40% reduction, virtual deadline is 60% of original)
/// - Low: 0.75 (25% reduction, virtual deadline is 75% of original)
/// - BestEffort: 1.0 (no scaling, uses original deadline - LATEST)
///
/// Lower scaling factors make virtual deadlines earlier, giving HIGH priority tasks the earliest
/// virtual deadlines. This strongly prioritizes high-priority tasks in the scheduling queue,
/// ensuring they are processed before lower-priority ones even when deadlines are similar.
///
/// # Returns
/// A PriorityTable mapping each priority to its scaling factor
fn scaling_table() -> PriorityTable<f64> {
    PriorityTable::from_fn(|priority| match priority {
        Priority::High => 0.01, // 95% reduction: High gets earliest virtual deadline (strongest priority)
        Priority::Medium => 0.8, // 40% reduction: Medium gets earlier virtual deadline
        Priority::Low => 0.9, // 25% reduction: Low gets earlier virtual deadline
        Priority::BestEffort => 1.0, // No scaling: BestEffort uses original deadline (lowest priority)
    })
}

pub struct GEDFVDScheduler {
    input_queues: PriorityTable<Arc<Receiver<Packet>>>,
    output_queues: PriorityTable<Sender<Packet>>,
    drop_counters: PriorityTable<Arc<AtomicU64>>,
    shared_queue: Arc<SharedQueue>,
    scaling: PriorityTable<f64>,
}

impl GEDFVDScheduler {
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
            scaling: scaling_table(),
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
        let dispatcher_inputs = self.input_queues.clone();
        let dispatcher_queue = self.shared_queue.clone();
        let dispatcher_running = running.clone();
        let scaling = self.scaling.clone();
        thread::Builder::new()
            .name("GEDF-VD-Dispatcher".to_string())
            .spawn(move || {
                set_priority(3);
                set_core(dispatcher_core);
                dispatcher_loop(
                    dispatcher_inputs,
                    dispatcher_queue,
                    scaling,
                    dispatcher_running,
                );
            })
            .expect("failed to spawn GEDF-VD dispatcher");

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
                .name(format!("GEDF-VD-Worker-{idx}"))
                .spawn(move || {
                    set_priority(3);
                    set_core(core_id);
                    worker_loop(worker_queue, worker_outputs, worker_drops, worker_running);
                })
                .expect("failed to spawn GEDF-VD worker thread");
        }
    }
}

/// Dispatcher loop: reads packets from input queues and pushes them to the shared queue with virtual deadlines.
///
/// The dispatcher:
/// 1. Iterates through priorities in order (High → Medium → Low → BestEffort)
/// 2. Tries to receive a packet from each priority's input queue (non-blocking)
/// 3. Calculates the virtual deadline: `timestamp + (latency_budget × scaling_factor)`
/// 4. Pushes the task to the shared queue (wakes a waiting worker)
/// 5. Yields if no packets were dispatched (prevents busy-waiting)
///
/// Virtual deadlines give HIGH priority tasks the earliest virtual deadlines (smallest scaling factor),
/// strongly prioritizing them in the scheduling queue. This ensures high-priority tasks are processed
/// before lower-priority ones even when deadlines are similar.
///
/// # Arguments
/// * `input_queues` - Per-priority input channels from ingress DRR
/// * `shared_queue` - Shared run queue (min-heap on virtual deadline)
/// * `scaling` - Per-priority scaling factors for virtual deadlines
/// * `running` - Atomic flag to signal shutdown
fn dispatcher_loop(
    input_queues: PriorityTable<Arc<Receiver<Packet>>>, // Input channels per priority
    shared_queue: Arc<SharedQueue>, // Shared run queue (min-heap on virtual deadline)
    scaling: PriorityTable<f64>, // Per-priority scaling factors (High=0.05, Medium=0.6, Low=0.75, BE=1.0) - INVERTED to prioritize HIGH
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
                    // Successfully received a packet: calculate virtual deadline and enqueue
                    // Calculate scaled latency budget: original budget × scaling factor
                    // Clamp scaling factor to [0.1, 1.0] to prevent extreme values
                    let scaled = packet
                        .latency_budget
                        .mul_f64(scaling[priority].clamp(0.1, 1.0));
                    // Calculate virtual deadline: ingress timestamp + scaled latency budget
                    // HIGHER priorities have smaller scaling factors (High=0.05, BE=1.0), so their virtual deadlines are earlier
                    // This INVERTED variant prioritizes high-priority tasks by giving them the earliest virtual deadlines
                    let virtual_deadline = packet.timestamp + scaled;
                    // Push task to shared queue (wakes a waiting worker if queue was empty)
                    shared_queue.push(QueuedTask {
                        virtual_deadline, // Virtual deadline (for heap ordering)
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
/// 1. Pops the earliest-virtual-deadline task from the shared queue (blocks if empty)
/// 2. Processes the packet (simulated CPU work, size-dependent)
/// 3. Forwards the packet to the appropriate output queue (or drops if full)
///
/// Multiple workers can run in parallel, all competing for tasks from the same shared queue.
/// This ensures true global EDF with virtual deadlines: the earliest virtual deadline across
/// all priorities is always processed first.
///
/// # Arguments
/// * `shared_queue` - Shared run queue (min-heap on virtual deadline)
/// * `output_queues` - Per-priority output channels to egress DRR
/// * `drop_counters` - Per-priority atomic drop counters (for metrics)
/// * `running` - Atomic flag to signal shutdown
fn worker_loop(
    shared_queue: Arc<SharedQueue>, // Shared run queue (min-heap on virtual deadline)
    output_queues: PriorityTable<Sender<Packet>>, // Output channels per priority
    drop_counters: PriorityTable<Arc<AtomicU64>>, // Drop counters per priority
    running: Arc<AtomicBool>, // Shutdown flag (passed to pop for early exit)
) {
    // Main worker loop: continues until shared_queue.pop returns None (shutdown)
    // The pop operation blocks if the queue is empty, and returns None on shutdown.
    while let Some(task) = shared_queue.pop(&running) {
        // Extract priority from the task (for routing to output queue)
        let priority = task.priority;

        // ========================================================================
        // STEP 1: Simulate packet processing (busy-wait)
        // ========================================================================
        // Calculate processing time based on packet size (0.05-0.15 ms)
        let processing_time = processing_duration(&task.packet);
        let start = Instant::now();
        // Busy-wait loop: spin until processing time has elapsed
        while start.elapsed() < processing_time {
            std::hint::spin_loop(); // CPU hint: indicates tight spin loop
        }

        // ========================================================================
        // STEP 2: Forward packet to output queue (or drop if full)
        // ========================================================================
        // Try to send to the egress DRR queue (non-blocking)
        if output_queues[priority].try_send(task.packet).is_err() {
            // Send failed (queue full): increment drop counter atomically
            // Relaxed ordering is sufficient: this is just a metric, not synchronization
            drop_counters[priority].fetch_add(1, AtomicOrdering::Relaxed);
        }
        // If send succeeded, packet is forwarded to egress DRR
    }
    // Loop exits when shared_queue.pop returns None (shutdown or queue closed)
}
