//! Earliest Deadline First processor.
//!
//! The EDF stage receives packets from per-priority bounded channels, keeps at most one head-of-line
//! packet buffered for each class, and always executes the task with the earliest absolute deadline.
//! Packets are then forwarded to the egress DRR stage while maintaining drop counters for metrics.

use crate::drr_scheduler::{Packet, Priority, PriorityTable};
use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use std::cmp::Ordering;
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

/// Drop counters exposed by the EDF stage.
#[derive(Debug, Clone)]
pub struct EDFDropCounters {
    /// Heap drops remain for compatibility; the current algorithm never fills a heap.
    pub heap: u64,
    /// Per priority drops recorded when the EDF → egress channel is full.
    pub output: PriorityTable<u64>,
}

/// EDF scheduler that keeps one buffered packet per priority and executes the earliest deadline.
pub struct EDFScheduler {
    input_queues: PriorityTable<Arc<Receiver<Packet>>>,
    output_queues: PriorityTable<Sender<Packet>>,
    output_drop_counters: PriorityTable<Arc<AtomicU64>>,
    /// Pending head-of-line packet for each priority.
    pending: Arc<Mutex<PriorityTable<Option<EDFTask>>>>,
    /// Retained for configuration compatibility; no heap is used in the current implementation.
    _max_heap_size: usize,
}

impl EDFScheduler {
    /// Construct a new EDF scheduler from per-priority channels and a heap size hint.
    pub fn new(
        input_queues: PriorityTable<Arc<Receiver<Packet>>>,
        output_queues: PriorityTable<Sender<Packet>>,
        max_heap_size: usize,
    ) -> Self {
        let output_drop_counters = PriorityTable::from_fn(|_| Arc::new(AtomicU64::new(0)));
        Self {
            input_queues,
            output_queues,
            output_drop_counters,
            pending: Arc::new(Mutex::new(PriorityTable::from_fn(|_| None))),
            _max_heap_size: max_heap_size,
        }
    }

    /// Return the currently recorded drop counts (used by metrics).
    pub fn get_drop_counts(&self) -> EDFDropCounters {
        EDFDropCounters {
            heap: 0,
            output: PriorityTable::from_fn(|priority| {
                self.output_drop_counters[priority].load(AtomicOrdering::Relaxed)
            }),
        }
    }

    /// Enqueue a packet directly (used in unit tests to bypass channels).
    #[allow(dead_code)] // Used in tests
    pub fn enqueue_packet(
        &self,
        packet: Packet,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let deadline = packet.timestamp + packet.latency_budget;
        let task = EDFTask { packet, deadline };
        let priority = task.packet.priority;

        let mut pending = self.pending.lock();
        if let Some(existing) = pending[priority].as_ref() {
            if existing.deadline <= task.deadline {
                return Ok(());
            }
        }
        pending[priority] = Some(task);
        Ok(())
    }

    /// Process the next available packet (if any) and return it for testing convenience.
    pub fn process_next(&self) -> Option<Packet> {
        let mut buffers = self.pending.lock();

        // Fill empty buffers with the head packet from each input queue.
        for priority in Priority::ALL {
            if buffers[priority].is_none() {
                if let Ok(packet) = self.input_queues[priority].try_recv() {
                    let deadline = packet.timestamp + packet.latency_budget;
                    buffers[priority] = Some(EDFTask { packet, deadline });
                }
            }
        }

        // Determine the earliest deadline among the buffered packets.
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
            return None;
        };

        let task = buffers[selected_priority]
            .take()
            .expect("pending task must exist for selected priority");
        drop(buffers);

        let packet = task.packet;

        // Deterministic processing time based on payload size and priority class.

        let base_ms = 0.10;
        let extra_ms = if packet.len() > 200 {
            let clamped = packet.len().min(1500);
            0.1 * ((clamped - 200) as f64 / 1300.0)
        } else {
            0.0
        };

        let processing_time_ms = base_ms + extra_ms;
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
    }

    #[allow(dead_code)]
    pub fn has_tasks(&self) -> bool {
        {
            let pending = self.pending.lock();
            if Priority::ALL
                .iter()
                .any(|priority| pending[*priority].is_some())
            {
                return true;
            }
        }

        Priority::ALL
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
            let mut packet = Packet::new(priority, &data, latency_budget);
            packet.timestamp = timestamp;
            self.inputs[priority].send(packet).unwrap();
        }

        fn step(&self) {
            let _ = self.scheduler.process_next();
        }
    }

    #[test]
    fn scheduler_initializes_empty() {
        let harness = SchedulerHarness::new(4, 16);
        let pending = harness.scheduler.pending.lock();
        assert!(Priority::ALL
            .iter()
            .all(|priority| pending[*priority].is_none()));
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
