use criterion::{black_box, criterion_group, criterion_main, Criterion};
use crossbeam_channel::unbounded;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aiedf_pipeline::drr_scheduler::{DRRScheduler, Packet};
use aiedf_pipeline::edf_scheduler::EDFScheduler;
use aiedf_pipeline::queue::Queue;

fn bench_drr_scheduler(c: &mut Criterion) {
    let mut group = c.benchmark_group("drr_scheduler");

    group.bench_function("schedule_packet", |b| {
        let (tx, _rx) = unbounded();
        let scheduler = Arc::new(DRRScheduler::new(tx));
        scheduler.add_flow(1, 1024, Duration::from_millis(1));

        let packet = Packet::new(
            aiedf_pipeline::drr_scheduler::Priority::High,
            vec![0u8; 100],
            Duration::from_millis(1),
        );

        b.iter(|| {
            scheduler
                .schedule_packet(black_box(packet.clone()))
                .unwrap();
        });
    });

    group.bench_function("add_flow", |b| {
        let (tx, _rx) = unbounded();
        let scheduler = Arc::new(DRRScheduler::new(tx));

        b.iter(|| {
            scheduler.add_flow(black_box(1), 1024, Duration::from_millis(1));
        });
    });
}

fn bench_edf_scheduler(c: &mut Criterion) {
    let mut group = c.benchmark_group("edf_scheduler");

    group.bench_function("enqueue_packet", |b| {
        let (tx1, rx1) = unbounded();
        let (tx2, _rx2) = unbounded();
        let scheduler = Arc::new(EDFScheduler::new(Arc::new(Mutex::new(rx1)), tx2));

        let packet = Packet::new(
            aiedf_pipeline::drr_scheduler::Priority::High,
            vec![0u8; 100],
            Duration::from_millis(1),
        );

        b.iter(|| {
            scheduler.enqueue_packet(black_box(packet.clone())).unwrap();
        });
    });

    group.bench_function("process_next", |b| {
        let (tx1, rx1) = unbounded();
        let (tx2, _rx2) = unbounded();
        let scheduler = Arc::new(EDFScheduler::new(Arc::new(Mutex::new(rx1)), tx2));

        // Pre-populate with packets
        for i in 0..100 {
            let priority = match i % 3 {
                0 => aiedf_pipeline::drr_scheduler::Priority::High,
                1 => aiedf_pipeline::drr_scheduler::Priority::Medium,
                _ => aiedf_pipeline::drr_scheduler::Priority::Low,
            };
            let packet = Packet::new(
                priority,
                vec![0u8; 100],
                Duration::from_millis(i as u64 % 100 + 1),
            );
            scheduler.enqueue_packet(packet).unwrap();
        }

        b.iter(|| {
            black_box(scheduler.process_next());
        });
    });
}

fn bench_queue(c: &mut Criterion) {
    let mut group = c.benchmark_group("queue");

    group.bench_function("send_recv", |b| {
        let queue = Arc::new(Queue::new());

        let packet = Packet::new(
            aiedf_pipeline::drr_scheduler::Priority::High,
            vec![0u8; 100],
            Duration::from_millis(1),
        );

        b.iter(|| {
            queue.send(black_box(packet.clone())).unwrap();
            black_box(queue.try_recv().unwrap());
        });
    });
}

criterion_group!(
    benches,
    bench_drr_scheduler,
    bench_edf_scheduler,
    bench_queue
);
criterion_main!(benches);
