use crossbeam_channel::unbounded;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aiedf_pipeline::drr_scheduler::{DRRScheduler, Packet};
use aiedf_pipeline::edf_scheduler::EDFScheduler;
use aiedf_pipeline::queue::Queue;

#[test]
fn test_pipeline_flow() {
    // Create queues
    let queue1 = Arc::new(Queue::new());
    let queue2 = Arc::new(Queue::new());

    // Create DRR scheduler
    let (tx1, rx1) = unbounded();
    let input_drr = Arc::new(DRRScheduler::new(tx1));
    input_drr.set_receiver(rx1);

    // Create EDF scheduler
    let (tx2, _rx2) = unbounded();
    let edf = Arc::new(EDFScheduler::new(queue1.receiver(), tx2));

    // Add flows
    input_drr.add_flow(1, 1024, Duration::from_millis(1));
    input_drr.add_flow(2, 1024, Duration::from_millis(50));
    input_drr.add_flow(3, 1024, Duration::from_millis(100));

    // Create and schedule packets
    let now = Instant::now();
    let packets = vec![
        Packet {
            flow_id: 1,
            data: vec![1, 2, 3],
            timestamp: now,
            latency_budget: Duration::from_millis(1),
        },
        Packet {
            flow_id: 2,
            data: vec![4, 5, 6],
            timestamp: now,
            latency_budget: Duration::from_millis(50),
        },
        Packet {
            flow_id: 3,
            data: vec![7, 8, 9],
            timestamp: now,
            latency_budget: Duration::from_millis(100),
        },
    ];

    // Send packets through DRR -> Queue1
    for packet in &packets {
        input_drr.schedule_packet(packet.clone()).unwrap();
        // Receive from DRR output and send to queue1
        if let Ok(p) = input_drr.process_next() {
            queue1.send(p).unwrap();
        }
    }

    // Process through EDF
    let mut processed = Vec::new();
    for _ in 0..3 {
        if let Some(packet) = edf.process_next() {
            processed.push(packet);
        }
    }

    // EDF should process in deadline order: flow 1, then 2, then 3
    assert_eq!(processed.len(), 3);
    assert_eq!(processed[0].flow_id, 1); // Earliest deadline
    assert_eq!(processed[1].flow_id, 2);
    assert_eq!(processed[2].flow_id, 3);
}

#[test]
fn test_drr_multiple_flows() {
    let (tx, rx) = unbounded();
    let scheduler = Arc::new(DRRScheduler::new(tx));
    scheduler.set_receiver(rx);

    scheduler.add_flow(1, 100, Duration::from_millis(1));
    scheduler.add_flow(2, 200, Duration::from_millis(50));

    let now = Instant::now();
    let packet1 = Packet {
        flow_id: 1,
        data: vec![0u8; 50],
        timestamp: now,
        latency_budget: Duration::from_millis(1),
    };

    let packet2 = Packet {
        flow_id: 2,
        data: vec![0u8; 100],
        timestamp: now,
        latency_budget: Duration::from_millis(50),
    };

    scheduler.schedule_packet(packet1.clone()).unwrap();
    scheduler.schedule_packet(packet2.clone()).unwrap();

    // Process packets
    let p1 = scheduler.process_next();
    let p2 = scheduler.process_next();

    assert!(p1.is_some());
    assert!(p2.is_some());
}
