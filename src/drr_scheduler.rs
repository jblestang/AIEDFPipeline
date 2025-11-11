//! Legacy DRR module that now re-exports the shared scheduling primitives.
//!
//! The actual data structures live in `priority.rs` and `packet.rs`, but we keep these re-exports to
//! minimise churn for downstream users that still depend on the historical module path.

#[allow(unused_imports)]
pub use crate::packet::{Packet, MAX_PACKET_SIZE};
#[allow(unused_imports)]
pub use crate::priority::{Priority, PriorityTable};
