use std::time::Duration;

use aiedf_pipeline::drr_scheduler::{Packet, Priority};

#[test]
fn packet_priority_roundtrip() {
    let packet = Packet::new(Priority::High, vec![1, 2, 3], Duration::from_millis(1));
    assert_eq!(packet.priority, Priority::High);
    assert_eq!(packet.flow_id, Priority::High.flow_id());
}
