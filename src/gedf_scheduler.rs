//! Global EDF scheduler (G-EDF) with shared run queue across worker threads.
use crate::drr_scheduler::{Packet, Priority, PriorityTable};
use crate::edf_scheduler::EDFDropCounters;
use crate::multi_worker_edf::processing_duration;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use parking_lot::{Condvar, Mutex};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

#[derive(Debug)]
struct QueuedTask {
    deadline: Instant,
    priority: Priority,
    packet: Packet,
}

impl Ord for QueuedTask {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse comparison so BinaryHeap pops the earliest deadline first.
        other.deadline.cmp(&self.deadline)
    }
}

impl PartialOrd for QueuedTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for QueuedTask {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline
    }
}

impl Eq for QueuedTask {}

struct SharedQueue {
    heap: Mutex<BinaryHeap<QueuedTask>>,
    available: Condvar,
}

impl SharedQueue {
    fn new() -> Self {
        Self {
            heap: Mutex::new(BinaryHeap::new()),
            available: Condvar::new(),
        }
    }

    fn push(&self, task: QueuedTask) {
        {
            let mut guard = self.heap.lock();
            guard.push(task);
        }
        self.available.notify_one();
    }

    fn pop(&self, running: &AtomicBool) -> Option<QueuedTask> {
        let mut guard = self.heap.lock();
        loop {
            if let Some(task) = guard.pop() {
                return Some(task);
            }
            if !running.load(AtomicOrdering::Relaxed) {
                return None;
            }
            self.available.wait(&mut guard);
        }
    }

    fn wake_all(&self) {
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

fn dispatcher_loop(
    input_queues: PriorityTable<Arc<Receiver<Packet>>>,
    shared_queue: Arc<SharedQueue>,
    running: Arc<AtomicBool>,
) {
    while running.load(AtomicOrdering::Relaxed) {
        let mut dispatched = false;
        for priority in Priority::ALL {
            match input_queues[priority].try_recv() {
                Ok(packet) => {
                    let deadline = packet.timestamp + packet.latency_budget;
                    shared_queue.push(QueuedTask {
                        deadline,
                        priority,
                        packet,
                    });
                    dispatched = true;
                }
                Err(TryRecvError::Empty) => continue,
                Err(TryRecvError::Disconnected) => continue,
            }
        }

        if !dispatched {
            thread::yield_now();
        }
    }

    shared_queue.wake_all();
}

fn worker_loop(
    shared_queue: Arc<SharedQueue>,
    output_queues: PriorityTable<Sender<Packet>>,
    drop_counters: PriorityTable<Arc<AtomicU64>>,
    running: Arc<AtomicBool>,
) {
    while let Some(task) = shared_queue.pop(&running) {
        let packet = task.packet;
        let priority = task.priority;

        let processing_time = processing_duration(&packet);
        let start = Instant::now();
        while start.elapsed() < processing_time {
            std::hint::spin_loop();
        }

        if output_queues[priority].try_send(packet).is_err() {
            drop_counters[priority].fetch_add(1, AtomicOrdering::Relaxed);
        }
    }
}
