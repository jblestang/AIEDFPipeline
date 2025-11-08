use crate::drr_scheduler::{Packet, Priority, PriorityTable};
use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone)]
struct EDFTask {
    packet: Packet,
    deadline: Instant,
}

impl Ord for EDFTask {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse order for min-heap (earliest deadline first)
        other.deadline.cmp(&self.deadline)
    }
}

impl PartialOrd for EDFTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for EDFTask {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline
    }
}

impl Eq for EDFTask {}

#[derive(Debug, Clone)]
pub struct EDFDropCounters {
    pub heap: u64,
    pub output: PriorityTable<u64>,
}

pub struct EDFScheduler {
    tasks: Arc<Mutex<BinaryHeap<EDFTask>>>,
    input_queues: PriorityTable<Arc<Receiver<Packet>>>,
    output_queues: PriorityTable<Sender<Packet>>,
    heap_drops: Arc<AtomicU64>,
    output_drop_counters: PriorityTable<Arc<AtomicU64>>,
    max_heap_size: usize,
}

impl EDFScheduler {
    pub fn new(
        input_queues: PriorityTable<Arc<Receiver<Packet>>>,
        output_queues: PriorityTable<Sender<Packet>>,
        max_heap_size: usize,
    ) -> Self {
        let output_drop_counters = PriorityTable::from_fn(|_| Arc::new(AtomicU64::new(0)));
        Self {
            tasks: Arc::new(Mutex::new(BinaryHeap::new())),
            input_queues,
            output_queues,
            heap_drops: Arc::new(AtomicU64::new(0)),
            output_drop_counters,
            max_heap_size,
        }
    }

    /// Get drop counts (for metrics)
    pub fn get_drop_counts(&self) -> EDFDropCounters {
        EDFDropCounters {
            heap: self.heap_drops.load(AtomicOrdering::Relaxed),
            output: PriorityTable::from_fn(|priority| {
                self.output_drop_counters[priority].load(AtomicOrdering::Relaxed)
            }),
        }
    }

    #[allow(dead_code)] // Used in tests
    pub fn enqueue_packet(
        &self,
        packet: Packet,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let deadline = packet.timestamp + packet.latency_budget;
        let task = EDFTask { packet, deadline };

        let mut tasks = self.tasks.lock();
        tasks.push(task);
        Ok(())
    }

    pub fn process_next(&self) -> Option<Packet> {
        let mut buffers: PriorityTable<Option<EDFTask>> = PriorityTable::from_fn(|_| None);
        let mut tasks_to_insert = Vec::with_capacity(self.max_heap_size);

        loop {
            for priority in Priority::ALL {
                if buffers[priority].is_none() {
                    if let Ok(packet) = self.input_queues[priority].try_recv() {
                        let deadline = packet.timestamp + packet.latency_budget;
                        *buffers.get_mut(priority) = Some(EDFTask { packet, deadline });
                    }
                }
            }

            let mut earliest: Option<(Priority, Instant)> = None;
            for priority in Priority::ALL {
                if let Some(task) = buffers[priority].as_ref() {
                    match earliest {
                        None => earliest = Some((priority, task.deadline)),
                        Some((_, current_deadline)) if task.deadline < current_deadline => {
                            earliest = Some((priority, task.deadline));
                        }
                        _ => {}
                    }
                }
            }

            let Some((selected_priority, _)) = earliest else {
                break;
            };

            let task = buffers
                .get_mut(selected_priority)
                .take()
                .expect("buffer must contain task before take()");
            tasks_to_insert.push(task);
        }

        let mut tasks = match self.tasks.try_lock() {
            Some(guard) => guard,
            None => return None,
        };
        if !tasks_to_insert.is_empty() {
            tasks.reserve(tasks_to_insert.len());
        }

        for task in tasks_to_insert {
            if tasks.len() >= self.max_heap_size {
                self.heap_drops.fetch_add(1, AtomicOrdering::Relaxed);
                if task.packet.priority == Priority::High {
                    if let Some(peeked) = tasks.peek() {
                        if matches!(peeked.packet.priority, Priority::Low | Priority::BestEffort) {
                            let _ = tasks.pop();
                        } else {
                            let _ = tasks.pop();
                        }
                    } else {
                        let _ = tasks.pop();
                    }
                } else {
                    let _ = tasks.pop();
                }
            }
            tasks.push(task);
        }

        if let Some(task) = tasks.pop() {
            drop(tasks);

            let packet = task.packet;

            let hash = packet
                .data
                .iter()
                .fold(0u64, |acc, &b| acc.wrapping_mul(31).wrapping_add(b as u64));

            let normalized = (hash % 1000) as f64 / 1000.0;

            let base_ms = 0.2;
            let extra_ms = if packet.data.len() > 200 {
                base_ms * ((packet.data.len().min(1400) - 200) as f64 / 1200.0)
            } else {
                0.0
            };

            let priority_offset = match packet.priority {
                Priority::High => normalized * 0.002,
                Priority::Medium => normalized * 0.004,
                Priority::Low | Priority::BestEffort => normalized * 0.006,
            };

            let processing_time_ms = base_ms + extra_ms + priority_offset;
            let processing_time = std::time::Duration::from_secs_f64(processing_time_ms / 1000.0);

            let start = Instant::now();
            while start.elapsed() < processing_time {
                std::hint::spin_loop();
            }

            let priority = packet.priority;
            if self.output_queues[priority].try_send(packet).is_err() {
                self.output_drop_counters[priority].fetch_add(1, AtomicOrdering::Relaxed);
            }
            None
        } else {
            None
        }
    }

    #[allow(dead_code)]
    pub fn has_tasks(&self) -> bool {
        !self.tasks.lock().is_empty()
            || Priority::ALL
                .iter()
                .any(|priority| !self.input_queues[*priority].is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;
    use std::time::{Duration, Instant};

    struct SchedulerHarness {
        scheduler: EDFScheduler,
        inputs: PriorityTable<crossbeam_channel::Sender<Packet>>,
        outputs: PriorityTable<crossbeam_channel::Receiver<Packet>>,
    }

    impl SchedulerHarness {
        fn new(capacity: usize, max_heap: usize) -> Self {
            let mut input_senders = Vec::new();
            let mut input_receivers = Vec::new();
            for _ in Priority::ALL {
                let (tx, rx) = bounded(capacity);
                input_senders.push(tx);
                input_receivers.push(Arc::new(rx));
            }

            let mut output_senders = Vec::new();
            let mut output_receivers = Vec::new();
            for _ in Priority::ALL {
                let (tx, rx) = bounded(capacity);
                output_senders.push(tx);
                output_receivers.push(rx);
            }

            let scheduler = EDFScheduler::new(
                PriorityTable::from_vec(input_receivers),
                PriorityTable::from_vec(output_senders),
                max_heap,
            );

            Self {
                scheduler,
                inputs: PriorityTable::from_vec(input_senders),
                outputs: PriorityTable::from_vec(output_receivers),
            }
        }

        fn enqueue(
            &self,
            priority: Priority,
            data: Vec<u8>,
            latency_budget: Duration,
            timestamp: Instant,
        ) {
            let mut packet = Packet::new(priority, data, latency_budget);
            packet.timestamp = timestamp;
            self.inputs[priority].send(packet).unwrap();
        }

        fn step(&self) {
            let _ = EDFScheduler::process_next(&self.scheduler);
        }
    }

    #[test]
    fn scheduler_initializes_empty() {
        let harness = SchedulerHarness::new(4, 16);
        assert!(harness.scheduler.tasks.lock().is_empty());
    }

    #[test]
    fn scheduler_processes_single_packet() {
        let harness = SchedulerHarness::new(4, 16);
        let now = Instant::now();
        harness.enqueue(Priority::High, vec![1], Duration::from_millis(10), now);
        harness.step();

        let mut observed = Vec::new();
        for priority in Priority::ALL {
            while let Ok(packet) = harness.outputs[priority].try_recv() {
                observed.push(packet.priority);
            }
        }
        assert_eq!(observed, vec![Priority::High]);
    }
}
