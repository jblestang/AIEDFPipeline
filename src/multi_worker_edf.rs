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

pub(crate) fn processing_duration(packet: &Packet) -> Duration {
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

fn run_worker(
    worker_id: usize,
    priorities: Vec<Priority>,
    input_queues: PriorityTable<Arc<Receiver<Packet>>>,
    completion: Arc<CompletionRouter>,
    running: Arc<AtomicBool>,
    backlog: Arc<AtomicUsize>,
    shared_priority_counters: PriorityTable<Arc<AtomicUsize>>,
    controller: Arc<AdaptiveController>,
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

    while running.load(AtomicOrdering::Relaxed) {
        let total_limit = controller.total_limit(worker_id);
        let per_priority_limit = controller.priority_limits(worker_id);
        // 1) Drain all eligible input queues while respecting per-priority + global quotas.
        //    Each admitted packet receives an EDF deadline and is inserted into the heap if its
        //    estimated finish time is still within the latency budget.
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
        if priorities.contains(&Priority::High)
            && !(scheduled.work_item.priority == Priority::Medium && counts[Priority::High] > 0)
        {
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
                controller.guard_threshold(scheduled.work_item.priority, packet.latency_budget)
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
                        let guard_window = remaining_guard.min(controller.guard_slice());
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
        let priority = scheduled.work_item.priority;
        controller.observe_processing_time(priority, processing_time);

        // 6) Emit the packet through the completion router. Sequence tracking guarantees
        //    per-priority FIFO order even though multiple workers run in parallel.
        completion.complete(scheduled.work_item);
    }

    backlog.store(0, AtomicOrdering::Relaxed);
}
