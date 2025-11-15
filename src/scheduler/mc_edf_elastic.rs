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
use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
// Import mutex for synchronization
use parking_lot::Mutex;
// Import ordering trait for deadline comparisons
use std::cmp::Ordering;
// Import BinaryHeap for EDF min-heap
use std::collections::BinaryHeap;
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
}

/// EDF scheduler for a single priority channel.
struct ChannelEDF {
    /// Min-heap of tasks ordered by deadline (earliest first)
    heap: Arc<Mutex<BinaryHeap<EDFTask>>>,
    /// Input channel for this EDF
    input: Arc<Receiver<EDFTask>>,
    /// Output channel for processed packets (goes to re-ordering stage)
    output: Sender<Packet>,
}

impl ChannelEDF {
    fn new(input: Arc<Receiver<EDFTask>>, output: Sender<Packet>) -> Self {
        Self {
            heap: Arc::new(Mutex::new(BinaryHeap::new())),
            input,
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

/// Multi-Channel EDF with Elasticity Scheduler
pub struct MCEDFElasticScheduler {
    /// Per-priority input channels from ingress DRR
    input_queues: PriorityTable<Arc<Receiver<Packet>>>,
    /// Per-priority output channels to egress DRR
    output_queues: PriorityTable<Sender<Packet>>,
    /// Drop counters for metrics
    drop_counters: PriorityTable<Arc<AtomicU64>>,
    /// Internal channels: HIGH EDF (can receive HIGH packets)
    high_channel_tx: Sender<EDFTask>,
    high_channel_rx: Arc<Receiver<EDFTask>>,
    /// Internal channels: MEDIUM EDF (can receive HIGH and MEDIUM packets)
    medium_channel_tx: Sender<EDFTask>,
    medium_channel_rx: Arc<Receiver<EDFTask>>,
    /// Internal channels: LOW EDF (can receive HIGH and LOW packets)
    low_channel_tx: Sender<EDFTask>,
    low_channel_rx: Arc<Receiver<EDFTask>>,
    /// Statistics for intelligent HIGH distribution across 3 EDFs
    high_distribution_stats: Arc<Mutex<HighDistributionStats>>,
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
        
        // Create channels for the 3 EDFs: HIGH, MEDIUM, LOW
        // HIGH can be distributed to all 3 EDFs to avoid favoring HIGH during bursts
        let (high_tx, high_rx) = unbounded();
        let (medium_tx, medium_rx) = unbounded();
        let (low_tx, low_rx) = unbounded();
        
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
        let edf_output_txs = [
            high_edf_out_tx,
            medium_edf_out_tx,
            low_edf_out_tx,
        ];
        
        let high_distribution_stats = Arc::new(Mutex::new(HighDistributionStats::new()));
        
        Self {
            input_queues,
            output_queues: output_queues.clone(),
            drop_counters,
            high_channel_tx: high_tx,
            high_channel_rx: Arc::new(high_rx),
            medium_channel_tx: medium_tx,
            medium_channel_rx: Arc::new(medium_rx),
            low_channel_tx: low_tx,
            low_channel_rx: Arc::new(low_rx),
            high_distribution_stats,
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
        // HIGH EDF: receives HIGH packets
        let high_edf = ChannelEDF::new(
            self.high_channel_rx.clone(),
            self.edf_output_txs[0].clone(),
        );
        // MEDIUM EDF: receives HIGH and MEDIUM packets
        let medium_edf = ChannelEDF::new(
            self.medium_channel_rx.clone(),
            self.edf_output_txs[1].clone(),
        );
        // LOW EDF: receives HIGH and LOW packets
        let low_edf = ChannelEDF::new(
            self.low_channel_rx.clone(),
            self.edf_output_txs[2].clone(),
        );

        // Spawn dispatcher thread (intelligent Round Robin for HIGH across 3 EDFs)
        let dispatcher_inputs = self.input_queues.clone();
        let dispatcher_high_tx = self.high_channel_tx.clone();
        let dispatcher_medium_tx = self.medium_channel_tx.clone();
        let dispatcher_low_tx = self.low_channel_tx.clone();
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
        
        // All EDFs run on the same core (first worker core)
        let edf_core = worker_cores[0];
        
        // HIGH EDF (receives HIGH packets only)
        let high_edf_heap = high_edf.heap.clone();
        let high_edf_input = high_edf.input.clone();
        let high_edf_output = high_edf.output.clone();
        let high_edf_drops = self.drop_counters[Priority::High].clone();
        let worker_running_high = worker_running.clone();
        thread::Builder::new()
            .name("MC-EDF-HIGH".to_string())
            .spawn(move || {
                set_priority(3);
                set_core(edf_core);
                edf_worker_loop(
                    high_edf_heap,
                    high_edf_input,
                    high_edf_output,
                    Priority::High,
                    high_edf_drops,
                    worker_running_high,
                    false, // HIGH has no elasticity
                );
            })
            .expect("failed to spawn MC-EDF HIGH worker");

        // MEDIUM EDF (receives HIGH and MEDIUM packets)
        let medium_edf_heap = medium_edf.heap.clone();
        let medium_edf_input = medium_edf.input.clone();
        let medium_edf_output = medium_edf.output.clone();
        let medium_edf_drops = self.drop_counters[Priority::Medium].clone();
        let worker_running_medium = worker_running.clone();
        thread::Builder::new()
            .name("MC-EDF-MEDIUM".to_string())
            .spawn(move || {
                set_priority(3);
                set_core(edf_core);
                edf_worker_loop(
                    medium_edf_heap,
                    medium_edf_input,
                    medium_edf_output,
                    Priority::Medium,
                    medium_edf_drops,
                    worker_running_medium,
                    true, // MEDIUM has elasticity
                );
            })
            .expect("failed to spawn MC-EDF MEDIUM worker");

        // LOW EDF (receives HIGH and LOW packets)
        let low_edf_heap = low_edf.heap.clone();
        let low_edf_input = low_edf.input.clone();
        let low_edf_output = low_edf.output.clone();
        let low_edf_drops = self.drop_counters[Priority::Low].clone();
        let worker_running_low = worker_running.clone();
        thread::Builder::new()
            .name("MC-EDF-LOW".to_string())
            .spawn(move || {
                set_priority(3);
                set_core(edf_core);
                edf_worker_loop(
                    low_edf_heap,
                    low_edf_input,
                    low_edf_output,
                    Priority::Low,
                    low_edf_drops,
                    worker_running_low,
                    true, // LOW has elasticity
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
                set_core(edf_core);
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
        Priority::Medium => 0.5,  // Divide by 2
        _ => 1.0,                 // No reduction
    }
}

/// Dispatcher loop: distributes packets to appropriate EDF channels.
///
/// HIGH priority packets are distributed using intelligent Round Robin across 3 EDF channels
/// (HIGH, MEDIUM, LOW) to avoid favoring HIGH during bursts.
/// MEDIUM packets go to MEDIUM EDF, LOW packets go to LOW EDF.
/// Virtual deadlines are calculated to prioritize HIGH (divide by 16) and MEDIUM (divide by 2).
fn dispatcher_loop(
    input_queues: PriorityTable<Arc<Receiver<Packet>>>,
    high_tx: Sender<EDFTask>,
    medium_tx: Sender<EDFTask>,
    low_tx: Sender<EDFTask>,
    distribution_stats: Arc<Mutex<HighDistributionStats>>,
    running: Arc<AtomicBool>,
) {
    while running.load(AtomicOrdering::Relaxed) {
        let now = Instant::now();

        // Process ALL HIGH priority packets first with intelligent Round Robin across 3 EDFs
        // This ensures HIGH packets are dispatched before any MEDIUM/LOW
        loop {
            match input_queues[Priority::High].try_recv() {
                Ok(packet) => {
                    let real_deadline = packet.timestamp + packet.latency_budget;
                    // Calculate virtual deadline: divide budget by 16 for HIGH (very aggressive)
                    let scale = virtual_deadline_scale(Priority::High);
                    let virtual_budget = packet.latency_budget.mul_f64(scale);
                    let virtual_deadline = packet.timestamp + virtual_budget;
                    
                    let task = EDFTask {
                        packet,
                        virtual_deadline,
                        real_deadline,
                        priority: Priority::High,
                    };

                    // Use intelligent Round Robin to select best EDF (0=HIGH, 1=MEDIUM, 2=LOW)
                    // Use real deadline for distribution stats
                    // Determine channel first, then clone and send (avoid holding lock during send)
                    let channel = {
                        let stats = distribution_stats.lock();
                        stats.find_best_channel(real_deadline, now)
                    };
                    
                    // Clone only once, after channel is determined
                    // Send to selected EDF (only one clone per packet)
                    let sent = match channel {
                        0 => high_tx.send(task.clone()).is_ok(),
                        1 => medium_tx.send(task.clone()).is_ok(),
                        2 => low_tx.send(task.clone()).is_ok(),
                        _ => false,
                    };
                    
                    if sent {
                        // Record distribution for statistics (using real deadline)
                        // Re-acquire lock only if send succeeded
                        let mut stats = distribution_stats.lock();
                        stats.record_distribution(channel, real_deadline, now);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
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
            let _ = medium_tx.send(task);
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
            let _ = low_tx.send(task);
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
            let _ = low_tx.send(task);
        }

        // Yield if no work
        thread::yield_now();
    }
}

/// EDF worker loop for a single channel.
///
/// Processes tasks from the EDF heap, applying elasticity if enabled.
/// Elasticity window is fixed at 120µs for MEDIUM/LOW packets.
fn edf_worker_loop(
    heap: Arc<Mutex<BinaryHeap<EDFTask>>>,
    input: Arc<Receiver<EDFTask>>,
    output: Sender<Packet>,
    _priority: Priority,
    drop_counter: Arc<AtomicU64>,
    running: Arc<AtomicBool>,
    has_elasticity: bool,
) {
    // Fixed elasticity window for MEDIUM/LOW
    let elasticity = Duration::from_micros(500);

    while running.load(AtomicOrdering::Relaxed) {
        // Receive new tasks from input channel (prioritize HIGH)
        // If HIGH arrives, process it immediately without batching for lower latency
        let mut received_tasks = Vec::new();
        
        loop {
            match input.try_recv() {
                Ok(task) => {
                    if task.priority == Priority::High {
                        // HIGH arrived: push immediately and break to process
                        let mut heap_guard = heap.lock();
                        heap_guard.push(task);
                        drop(heap_guard);
                        break; // Process HIGH immediately
                    } else {
                        // Non-HIGH: batch for efficiency
                        received_tasks.push(task);
                    }
                }
                Err(_) => break,
            }
        }
        
        // Batch push non-HIGH tasks to heap (single lock acquisition)
        if !received_tasks.is_empty() {
            let mut heap_guard = heap.lock();
            for task in received_tasks {
                heap_guard.push(task);
            }
        }

        // Process task with earliest virtual deadline (HIGH will have earliest due to VD)
        let task = {
            let mut heap_guard = heap.lock();
            heap_guard.pop()
        };

        if let Some(task) = task {
            // Apply elasticity ONLY if the current packet being processed is NOT HIGH
            // HIGH packets should never be delayed by elasticity
            if has_elasticity && task.priority != Priority::High {
                let wait_start = Instant::now();
                let check_interval = Duration::from_micros(3); // Check every 3µs for HIGH (more frequent)
                let mut last_check = Instant::now();
                
                while wait_start.elapsed() < elasticity {
                    // Check for incoming HIGH packets frequently during elasticity window
                    if last_check.elapsed() >= check_interval {
                        if let Ok(new_task) = input.try_recv() {
                            // New packet arrived! Push it and current task back to heap
                            let mut heap_guard = heap.lock();
                            heap_guard.push(new_task);
                            heap_guard.push(task.clone());
                            drop(heap_guard);
                            // Break elasticity wait immediately and process earliest from heap
                            // If HIGH arrived, it will be processed next due to virtual deadline
                            continue;
                        }
                        last_check = Instant::now();
                    }
                    std::hint::spin_loop();
                }
            }

            // Process the packet
            let processing_time = processing_duration(&task.packet);
            let start = Instant::now();
            while start.elapsed() < processing_time {
                std::hint::spin_loop();
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
                }
            }
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
        } else {
            // No packets available, yield
            thread::yield_now();
        }
    }
}

