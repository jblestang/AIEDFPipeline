// Comprehensive unit tests for the pipeline

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use aiedf_pipeline::drr_scheduler::{Packet, Priority};
    use aiedf_pipeline::queue::Queue;

    #[test]
    fn test_queue_operations() {
        let queue = Queue::new();
        assert!(queue.is_empty());

        let packet = Packet::new(Priority::High, vec![1, 2, 3], Duration::from_millis(1));

        queue.send(packet.clone()).unwrap();
        assert!(!queue.is_empty());
        assert_eq!(queue.len(), 1);

        let received = queue.try_recv().unwrap();
        assert_eq!(received.priority, packet.priority);
        assert_eq!(received.data, packet.data);
    }
}
