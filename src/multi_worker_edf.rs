use crate::drr_scheduler::{Packet, Priority, PriorityTable};
use crate::edf_scheduler::EDFDropCounters;
use crossbeam_channel::{self, Receiver, Sender, TryRecvError};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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
const WORKER_LOCAL_CAPACITY: usize = 1;

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
    let base_ms = 0.1;
    let extra_ms = if packet.len() > 200 {
        let clamped = packet.len().min(1500);
        0.2 * ((clamped - 200) as f64 / 1300.0)
    } else {
        0.0
    };
    Duration::from_secs_f64((base_ms + extra_ms) / 1000.0)
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
}

#[derive(Debug, Clone)]
pub struct MultiWorkerStats {
    pub worker_queue_depths: Vec<usize>,
    pub worker_queue_capacity: usize,
    pub dispatcher_backlog: usize,
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

        Self {
            input_queues,
            completion: Arc::new(CompletionRouter::new(output_queues)),
            worker_backlogs,
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
            worker_queue_capacity: WORKER_LOCAL_CAPACITY,
            dispatcher_backlog: 0,
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

    let mut heap: BinaryHeap<ScheduledItem> = BinaryHeap::new();

    let push_with_capacity = |heap: &mut BinaryHeap<ScheduledItem>, item: ScheduledItem| {
        if heap.len() >= WORKER_LOCAL_CAPACITY {
            if let Some(worst) = heap.peek() {
                if item.deadline >= worst.deadline {
                    return;
                }
            }
            heap.pop();
        }
        heap.push(item);
    };

    while running.load(AtomicOrdering::Relaxed) {
        for &priority in &priorities {
            loop {
                match input_queues[priority].try_recv() {
                    Ok(packet) => {
                        let deadline = packet.timestamp + packet.latency_budget;
                        let work_item = completion.prepare(priority, packet, deadline);
                        push_with_capacity(
                            &mut heap,
                            ScheduledItem {
                                deadline,
                                work_item,
                            },
                        );
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }
        }

        backlog.store(heap.len(), AtomicOrdering::Relaxed);

        let mut scheduled = match heap.pop() {
            Some(item) => item,
            None => {
                thread::yield_now();
                continue;
            }
        };

        let mut preempted = false;
        for &priority in &priorities {
            while heap.len() < WORKER_LOCAL_CAPACITY {
                match input_queues[priority].try_recv() {
                    Ok(packet) => {
                        let deadline = packet.timestamp + packet.latency_budget;
                        let work_item = completion.prepare(priority, packet, deadline);
                        if deadline < scheduled.deadline {
                            push_with_capacity(&mut heap, scheduled);
                            scheduled = ScheduledItem {
                                deadline,
                                work_item,
                            };
                            preempted = true;
                            break;
                        } else {
                            push_with_capacity(
                                &mut heap,
                                ScheduledItem {
                                    deadline,
                                    work_item,
                                },
                            );
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }
            if preempted {
                break;
            }
        }

        backlog.store(heap.len(), AtomicOrdering::Relaxed);

        let processing_time = processing_duration(&scheduled.work_item.packet);
        let start = Instant::now();
        while start.elapsed() < processing_time {
            std::hint::spin_loop();
        }

        completion.complete(scheduled.work_item);
    }

    backlog.store(0, AtomicOrdering::Relaxed);
}
