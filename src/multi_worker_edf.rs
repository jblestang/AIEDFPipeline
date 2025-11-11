use crate::drr_scheduler::{Packet, Priority, PriorityTable};
use crate::edf_scheduler::EDFDropCounters;
use crossbeam_channel::{self, Receiver, Sender, TryRecvError};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// Worker 0: strictly HIGH, Worker 1: HIGH+MED, Worker 2: HIGH+MED+LOW+BE.
const WORKER_ASSIGNMENTS: [&[Priority]; 3] = [
    &[Priority::High],
    &[Priority::High, Priority::Medium],
    &[
        Priority::High,
        Priority::Medium,
        Priority::Low,
        Priority::BestEffort,
    ],
];

// Maximum time slice spent waiting for a late-arriving HIGH packet before re-evaluating.
const HIGH_GUARD_MAX_SLICE: Duration = Duration::from_micros(10);

// Per-worker admission ceilings. We intentionally keep worker 0 very small to act as an
// aggressive front-line drain for high-priority bursts. Worker 1 is the bulk medium handler,
// while worker 2 has wider limits because it absorbs the remaining backlog (including BE).
fn worker_total_limit(worker_id: usize) -> usize {
    match worker_id {
        0 => 16,
        1 => 64,
        _ => 256,
    }
}

// Fine-grained per-priority caps. By lowering HIGH quotas on workers 1 and 2 we ensure worker 0
// still participates and that MED/LOW loads cannot push HIGH completely out of the local heap.
// (The TOTAL limit is checked together with this table in push_with_capacity.)
fn worker_priority_limit(worker_id: usize, priority: Priority) -> usize {
    match worker_id {
        0 => match priority {
            Priority::High => 16,
            _ => 0,
        },
        1 => match priority {
            Priority::High => 4,
            Priority::Medium => 60,
            _ => 0,
        },
        _ => match priority {
            Priority::High => 4,
            Priority::Medium => 56,
            Priority::Low => 64,
            Priority::BestEffort => 128,
        },
    }
}

#[derive(Clone)]
struct WorkItem {
    priority: Priority,
    sequence: u64,
    packet: Packet,
}

struct SequenceInner {
    next_emit: u64,
    pending: BTreeMap<u64, Packet>,
}

struct SequenceTracker {
    next_sequence: AtomicU64,
    inner: Mutex<SequenceInner>,
}

impl SequenceTracker {
    fn new() -> Self {
        Self {
            next_sequence: AtomicU64::new(0),
            inner: Mutex::new(SequenceInner {
                next_emit: 0,
                pending: BTreeMap::new(),
            }),
        }
    }

    fn assign_sequence(&self) -> u64 {
        self.next_sequence.fetch_add(1, AtomicOrdering::Relaxed)
    }

    fn complete(
        &self,
        sequence: u64,
        packet: Packet,
        sender: &Sender<Packet>,
        drop_counter: &AtomicU64,
    ) {
        let mut inner = self.inner.lock().expect("sequence tracker poisoned");
        if sequence == inner.next_emit {
            send_packet(sender, packet, drop_counter);
            let mut next_emit = inner.next_emit + 1;
            while let Some(next_packet) = inner.pending.remove(&next_emit) {
                send_packet(sender, next_packet, drop_counter);
                next_emit += 1;
            }
            inner.next_emit = next_emit;
        } else {
            inner.pending.insert(sequence, packet);
        }
    }
}

fn send_packet(sender: &Sender<Packet>, packet: Packet, drop_counter: &AtomicU64) {
    if sender.try_send(packet).is_err() {
        drop_counter.fetch_add(1, AtomicOrdering::Relaxed);
    }
}

struct CompletionRouter {
    trackers: PriorityTable<Arc<SequenceTracker>>,
    output_queues: PriorityTable<Sender<Packet>>,
    drop_counters: PriorityTable<Arc<AtomicU64>>,
}

impl CompletionRouter {
    fn new(output_queues: PriorityTable<Sender<Packet>>) -> Self {
        Self {
            trackers: PriorityTable::from_fn(|_| Arc::new(SequenceTracker::new())),
            output_queues,
            drop_counters: PriorityTable::from_fn(|_| Arc::new(AtomicU64::new(0))),
        }
    }

    fn complete(&self, work_item: WorkItem) {
        let tracker = &self.trackers[work_item.priority];
        let sender = &self.output_queues[work_item.priority];
        let drop_counter = &self.drop_counters[work_item.priority];
        tracker.complete(work_item.sequence, work_item.packet, sender, drop_counter);
    }

    fn drop_counts(&self) -> PriorityTable<u64> {
        PriorityTable::from_fn(|priority| {
            self.drop_counters[priority].load(AtomicOrdering::Relaxed)
        })
    }

    fn prepare(&self, priority: Priority, packet: Packet, _deadline: Instant) -> WorkItem {
        let tracker = &self.trackers[priority];
        let sequence = tracker.assign_sequence();
        WorkItem {
            priority,
            sequence,
            packet,
        }
    }
}

fn processing_duration(packet: &Packet) -> Duration {
    // Base duration tuned for 0.05 ms when payload <= 200B and up to ~0.15 ms at MTU.
    let base_ms = 0.05;
    let extra_ms = if packet.len() > 200 {
        let clamped = packet.len().min(1500);
        0.1 * ((clamped - 200) as f64 / 1300.0)
    } else {
        0.0
    };
    Duration::from_secs_f64((base_ms + extra_ms) / 1000.0)
}

fn push_with_capacity(
    heap: &mut BinaryHeap<ScheduledItem>,
    counts: &mut PriorityTable<usize>,
    total_count: &mut usize,
    per_priority_limit: &PriorityTable<usize>,
    total_limit: usize,
    item: ScheduledItem,
) -> bool {
    // Reject immediately if this worker is not allowed to run the packet's priority.
    let priority = item.work_item.priority;
    let limit_for_priority = per_priority_limit[priority];
    if limit_for_priority == 0 {
        return false;
    }
    // We also enforce the global per-worker backlog ceiling to avoid runaway heaps.
    if *total_count >= total_limit || counts[priority] >= limit_for_priority {
        return false;
    }

    heap.push(item);

    counts[priority] += 1;
    *total_count += 1;
    true
}

#[derive(Clone)]
struct ScheduledItem {
    deadline: Instant,
    work_item: WorkItem,
}

impl PartialEq for ScheduledItem {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline
    }
}

impl Eq for ScheduledItem {}

impl PartialOrd for ScheduledItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScheduledItem {
    fn cmp(&self, other: &Self) -> Ordering {
        other.deadline.cmp(&self.deadline)
    }
}

pub struct MultiWorkerScheduler {
    input_queues: PriorityTable<Arc<Receiver<Packet>>>,
    completion: Arc<CompletionRouter>,
    worker_backlogs: Vec<Arc<AtomicUsize>>,
    worker_priority_counters: Vec<PriorityTable<Arc<AtomicUsize>>>,
}

#[derive(Debug, Clone)]
pub struct MultiWorkerStats {
    pub worker_queue_depths: Vec<usize>,
    pub worker_queue_capacities: Vec<usize>,
    pub dispatcher_backlog: usize,
    pub worker_priority_depths: Vec<Vec<usize>>,
}

impl MultiWorkerScheduler {
    pub fn new(
        input_queues: PriorityTable<Arc<Receiver<Packet>>>,
        output_queues: PriorityTable<Sender<Packet>>,
    ) -> Self {
        let worker_backlogs = WORKER_ASSIGNMENTS
            .iter()
            .map(|_| Arc::new(AtomicUsize::new(0)))
            .collect();
        let worker_priority_counters = WORKER_ASSIGNMENTS
            .iter()
            .map(|_| PriorityTable::from_fn(|_| Arc::new(AtomicUsize::new(0))))
            .collect();

        Self {
            input_queues,
            completion: Arc::new(CompletionRouter::new(output_queues)),
            worker_backlogs,
            worker_priority_counters,
        }
    }

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
                    );
                })
                .expect("failed to spawn EDF worker thread");
        }
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
                .map(|(worker_id, _)| worker_total_limit(worker_id))
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

fn run_worker(
    worker_id: usize,
    priorities: Vec<Priority>,
    input_queues: PriorityTable<Arc<Receiver<Packet>>>,
    completion: Arc<CompletionRouter>,
    running: Arc<AtomicBool>,
    backlog: Arc<AtomicUsize>,
    shared_priority_counters: PriorityTable<Arc<AtomicUsize>>,
) {
    let name = format!("EDF-Worker-{}", worker_id);
    if let Ok(name_cstr) = std::ffi::CString::new(name.clone()) {
        #[cfg(target_os = "macos")]
        unsafe {
            libc::pthread_setname_np(name_cstr.as_ptr());
        }
        #[cfg(target_os = "linux")]
        unsafe {
            libc::pthread_setname_np(libc::pthread_self(), name_cstr.as_ptr());
        }
    }

    // Each worker maintains its own min-deadline heap (BinaryHeap with reversed ordering).
    let mut heap: BinaryHeap<ScheduledItem> = BinaryHeap::new();
    // counts => number of packets enqueued for each priority inside the worker heap.
    let mut counts = PriorityTable::from_fn(|_| 0usize);
    // total_count => total packets currently admitted for that worker.
    let mut total_count = 0usize;
    let total_limit = worker_total_limit(worker_id);
    let per_priority_limit =
        PriorityTable::from_fn(|priority| worker_priority_limit(worker_id, priority));

    while running.load(AtomicOrdering::Relaxed) {
        // 1) Drain all eligible input queues while respecting per-priority + global quotas.
        //    Each admitted packet receives an EDF deadline and is inserted into the heap.
        for &priority in &priorities {
            loop {
                if counts[priority] >= per_priority_limit[priority] || total_count >= total_limit {
                    break;
                }
                match input_queues[priority].try_recv() {
                    Ok(packet) => {
                        let deadline = packet.timestamp + packet.latency_budget;
                        let work_item = completion.prepare(priority, packet, deadline);
                        let pushed = push_with_capacity(
                            &mut heap,
                            &mut counts,
                            &mut total_count,
                            &per_priority_limit,
                            total_limit,
                            ScheduledItem {
                                deadline,
                                work_item,
                            },
                        );
                        if pushed {
                            shared_priority_counters[priority]
                                .fetch_add(1, AtomicOrdering::Relaxed);
                        } else {
                            break;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }
        }

        // Update backlog so metrics/GUI can display per-worker queue depth.
        backlog.store(total_count, AtomicOrdering::Relaxed);

        // 2) Select the earliest-deadline item. Empty heap => worker idles (yields).
        let mut scheduled = match heap.pop() {
            Some(item) => item,
            None => {
                thread::yield_now();
                continue;
            }
        };
        let initial_priority = scheduled.work_item.priority;
        debug_assert!(counts[initial_priority] > 0);
        counts[initial_priority] -= 1;
        total_count -= 1;
        shared_priority_counters[initial_priority].fetch_sub(1, AtomicOrdering::Relaxed);

        // 3) Immediate preemption opportunity: before spinning, check for freshly-arrived HIGH
        //    packets. This minimizes the extra latency a burst incurs (they can displace the
        //    current job instantly instead of waiting for the busy loop to complete).
        if priorities.contains(&Priority::High) {
            loop {
                if counts[Priority::High] >= per_priority_limit[Priority::High]
                    || total_count >= total_limit
                {
                    break;
                }
                match input_queues[Priority::High].try_recv() {
                    Ok(packet) => {
                        let deadline = packet.timestamp + packet.latency_budget;
                        let work_item = completion.prepare(Priority::High, packet, deadline);

                        if deadline < scheduled.deadline {
                            // Requeue the current job and run the more urgent packet instead.
                            let requeued_priority = initial_priority;
                            let requeued = push_with_capacity(
                                &mut heap,
                                &mut counts,
                                &mut total_count,
                                &per_priority_limit,
                                total_limit,
                                scheduled,
                            );
                            debug_assert!(requeued, "failed to requeue preempted packet");
                            shared_priority_counters[requeued_priority]
                                .fetch_add(1, AtomicOrdering::Relaxed);

                            scheduled = ScheduledItem {
                                deadline,
                                work_item,
                            };
                            // scheduled_priority = Priority::High; // This line is removed
                        } else {
                            if push_with_capacity(
                                &mut heap,
                                &mut counts,
                                &mut total_count,
                                &per_priority_limit,
                                total_limit,
                                ScheduledItem {
                                    deadline,
                                    work_item,
                                },
                            ) {
                                shared_priority_counters[Priority::High]
                                    .fetch_add(1, AtomicOrdering::Relaxed);
                            } else {
                                break;
                            }
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }
        }

        // 4) Guard window based on "deadline aging": limit short waits to Medium/Low packets that are
        //    still early in their latency budget. This keeps HIGH latency tails low without stalling
        //    classes that are already close to their own deadline.
        if priorities.contains(&Priority::High) {
            let packet = &scheduled.work_item.packet;
            let elapsed = Instant::now().saturating_duration_since(packet.timestamp);
            if let Some(guard_threshold) =
                guard_threshold_for(scheduled.work_item.priority, packet.latency_budget)
            {
                if elapsed < guard_threshold {
                    let mut current_elapsed = elapsed;
                    'guard: while current_elapsed < guard_threshold {
                        let remaining_guard = guard_threshold
                            .checked_sub(current_elapsed)
                            .unwrap_or_default();
                        if remaining_guard.is_zero() {
                            break;
                        }
                        let guard_window = remaining_guard.min(HIGH_GUARD_MAX_SLICE);
                        let guard_deadline = Instant::now() + guard_window;
                        while Instant::now() < guard_deadline {
                            if counts[Priority::High] >= per_priority_limit[Priority::High]
                                || total_count >= total_limit
                            {
                                break;
                            }
                            match input_queues[Priority::High].try_recv() {
                                Ok(packet) => {
                                    let deadline = packet.timestamp + packet.latency_budget;
                                    let work_item =
                                        completion.prepare(Priority::High, packet, deadline);

                                    let requeued_priority = initial_priority;
                                    let requeued = push_with_capacity(
                                        &mut heap,
                                        &mut counts,
                                        &mut total_count,
                                        &per_priority_limit,
                                        total_limit,
                                        scheduled,
                                    );
                                    debug_assert!(requeued, "failed to requeue guarded packet");
                                    shared_priority_counters[requeued_priority]
                                        .fetch_add(1, AtomicOrdering::Relaxed);

                                    scheduled = ScheduledItem {
                                        deadline,
                                        work_item,
                                    };
                                    break 'guard;
                                }
                                Err(TryRecvError::Empty) => std::hint::spin_loop(),
                                Err(TryRecvError::Disconnected) => break 'guard,
                            }
                        }
                        current_elapsed =
                            Instant::now().saturating_duration_since(packet.timestamp);
                        if current_elapsed >= guard_threshold {
                            break;
                        }
                    }
                }
            }
        }

        // 5) Simulate the CPU work (busy wait). In production this would be real compute.
        let processing_time = processing_duration(&scheduled.work_item.packet);
        let start = Instant::now();
        while start.elapsed() < processing_time {
            std::hint::spin_loop();
        }

        // 6) Emit the packet through the completion router. Sequence tracking guarantees
        //    per-priority FIFO order even though multiple workers run in parallel.
        completion.complete(scheduled.work_item);
    }

    backlog.store(0, AtomicOrdering::Relaxed);
}

fn guard_threshold_for(priority: Priority, latency_budget: Duration) -> Option<Duration> {
    match priority {
        Priority::High => None,
        Priority::Medium => Some(latency_budget.mul_f64(0.05).min(Duration::from_micros(150))),
        Priority::Low => Some(latency_budget.mul_f64(0.02).min(Duration::from_micros(150))),
        Priority::BestEffort => None,
    }
}
