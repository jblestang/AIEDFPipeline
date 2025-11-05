// Comprehensive unit tests for the pipeline

#[cfg(test)]
mod tests {
    use crossbeam_channel::unbounded;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use aiedf_pipeline::drr_scheduler::{DRRScheduler, Packet};
    use aiedf_pipeline::edf_scheduler::EDFScheduler;
    use aiedf_pipeline::queue::Queue;

    #[test]
    fn test_queue_operations() {
        let queue = Queue::new();
        assert!(queue.is_empty());

        let packet = Packet {
            flow_id: 1,
            data: vec![1, 2, 3],
            timestamp: Instant::now(),
            latency_budget: Duration::from_millis(1),
        };

        queue.send(packet.clone()).unwrap();
        assert!(!queue.is_empty());
        assert_eq!(queue.len(), 1);

        let received = queue.try_recv().unwrap();
        assert_eq!(received.flow_id, packet.flow_id);
        assert_eq!(received.data, packet.data);
    }

    #[test]
    fn test_drr_scheduler_flow_management() {
        let (tx, _rx) = unbounded();
        let scheduler = Arc::new(DRRScheduler::new(tx));

        scheduler.add_flow(1, 1024, Duration::from_millis(1));
        scheduler.add_flow(2, 2048, Duration::from_millis(50));

        let flows = scheduler.flows.lock();
        assert_eq!(flows.len(), 2);
        assert!(flows.contains_key(&1));
        assert!(flows.contains_key(&2));
        assert_eq!(flows[&1].quantum, 1024);
        assert_eq!(flows[&2].quantum, 2048);
    }

    #[test]
    fn test_drr_scheduler_packet_scheduling() {
        let (tx, rx) = unbounded();
        let scheduler = Arc::new(DRRScheduler::new(tx));

        let packet = Packet {
            flow_id: 1,
            data: vec![1, 2, 3, 4, 5],
            timestamp: Instant::now(),
            latency_budget: Duration::from_millis(1),
        };

        scheduler.schedule_packet(packet.clone()).unwrap();

        // Packet should be sent to the output channel
        let received = rx.recv().unwrap();
        assert_eq!(received.flow_id, 1);
        assert_eq!(received.data, packet.data);
    }

    #[test]
    fn test_edf_scheduler_ordering() {
        let (tx1, rx1) = unbounded();
        let (tx2, _rx2) = unbounded();
        use parking_lot::Mutex;
        use std::sync::Arc;
        let scheduler = Arc::new(EDFScheduler::new(Arc::new(Mutex::new(rx1)), tx2));

        let now = Instant::now();

        // Create packets with different deadlines
        let packet1 = Packet {
            flow_id: 1,
            data: vec![1],
            timestamp: now,
            latency_budget: Duration::from_millis(100),
        };

        let packet2 = Packet {
            flow_id: 2,
            data: vec![2],
            timestamp: now,
            latency_budget: Duration::from_millis(50),
        };

        let packet3 = Packet {
            flow_id: 3,
            data: vec![3],
            timestamp: now,
            latency_budget: Duration::from_millis(1),
        };

        // Enqueue in wrong order
        scheduler.enqueue_packet(packet1.clone()).unwrap();
        scheduler.enqueue_packet(packet2.clone()).unwrap();
        scheduler.enqueue_packet(packet3.clone()).unwrap();

        // Should process in deadline order: 3, 2, 1
        let processed1 = scheduler.process_next().unwrap();
        assert_eq!(processed1.flow_id, 3); // Earliest deadline

        let processed2 = scheduler.process_next().unwrap();
        assert_eq!(processed2.flow_id, 2);

        let processed3 = scheduler.process_next().unwrap();
        assert_eq!(processed3.flow_id, 1);
    }

    #[test]
    fn test_edf_scheduler_with_receiver() {
        let (tx1, rx1) = unbounded();
        let (tx2, _rx2) = unbounded();
        use parking_lot::Mutex;
        use std::sync::Arc;
        let scheduler = Arc::new(EDFScheduler::new(Arc::new(Mutex::new(rx1)), tx2));

        let now = Instant::now();

        // Send packets through the input channel
        let packet1 = Packet {
            flow_id: 1,
            data: vec![1],
            timestamp: now,
            latency_budget: Duration::from_millis(100),
        };

        let packet2 = Packet {
            flow_id: 2,
            data: vec![2],
            timestamp: now,
            latency_budget: Duration::from_millis(1),
        };

        tx1.send(packet1.clone()).unwrap();
        tx1.send(packet2.clone()).unwrap();

        // Process should pick up packets from receiver and order by deadline
        let processed = scheduler.process_next().unwrap();
        assert_eq!(processed.flow_id, 2); // Earliest deadline (1ms)
    }

    #[test]
    fn test_pipeline_flow() {
        // Test the complete flow: Queue -> EDF -> Queue
        let queue1 = Arc::new(Queue::new());

        let (tx, _rx) = unbounded();
        use parking_lot::Mutex;
        use std::sync::Arc;
        let edf = Arc::new(EDFScheduler::new(queue1.receiver(), tx));

        let now = Instant::now();
        let packet = Packet {
            flow_id: 1,
            data: vec![1, 2, 3],
            timestamp: now,
            latency_budget: Duration::from_millis(1),
        };

        // Send to queue1
        queue1.send(packet.clone()).unwrap();

        // Process through EDF
        let processed = edf.process_next();
        assert!(processed.is_some());
        let processed_packet = processed.unwrap();
        assert_eq!(processed_packet.flow_id, 1);
    }
}
