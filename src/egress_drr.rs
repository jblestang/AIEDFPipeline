//! Egress Deficit Round Robin scheduler.
//!
//! This stage drains EDF output queues in strict priority order, records latency metrics, and emits
//! packets through UDP sockets. Metrics recording uses a lock-free channel so the hot path does not
//! contend with the statistics thread.

use crate::drr_scheduler::{Packet, Priority, PriorityTable};
use crossbeam_channel::TryRecvError;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Instant;

/// Egress DRR Scheduler - Reads from priority queues and writes to UDP sockets
pub struct EgressDRRScheduler {
    priority_receivers: PriorityTable<crossbeam_channel::Receiver<Packet>>,
    // Output socket map: priority -> (socket, address)
    output_sockets: Arc<Mutex<HashMap<Priority, (Arc<UdpSocket>, SocketAddr)>>>,
}

impl EgressDRRScheduler {
    /// Build a new egress scheduler using pre-created per-priority channels.
    pub fn new(priority_receivers: PriorityTable<crossbeam_channel::Receiver<Packet>>) -> Self {
        Self {
            priority_receivers,
            output_sockets: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register an output socket that will receive packets for a given priority class.
    pub fn add_output_socket(
        &self,
        priority: Priority,
        socket: Arc<UdpSocket>,
        address: SocketAddr,
    ) {
        socket
            .set_nonblocking(true)
            .expect("failed to set UDP socket to non-blocking");
        self.output_sockets
            .lock()
            .insert(priority, (socket, address));
    }

    /// Process packets from priority queues and send to output sockets
    pub async fn process_queues(
        &self,
        running: Arc<std::sync::atomic::AtomicBool>,
        metrics_collector: Arc<crate::metrics::MetricsCollector>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let receivers =
            PriorityTable::from_fn(|priority| self.priority_receivers[priority].clone());

        // Clone sockets at start to avoid locking within the hot loop.
        let socket_map: HashMap<Priority, (Arc<UdpSocket>, SocketAddr)> = {
            let sockets = self.output_sockets.lock();
            sockets.clone()
        };

        let handle = tokio::task::spawn_blocking(move || {
            let mut next_index = 0usize;
            let mut idle_cycles = 0usize;
            let priorities = Priority::ALL;
            let num_priorities = priorities.len();

            while running.load(std::sync::atomic::Ordering::Relaxed) {
                let priority = priorities[next_index];
                next_index = (next_index + 1) % num_priorities;

                match receivers[priority].try_recv() {
                    Ok(packet) => {
                        idle_cycles = 0;

                        // Record metrics before the attempt to send so dropped packets are still tracked.
                        let latency = packet.timestamp.elapsed();
                        let deadline = packet.timestamp + packet.latency_budget;
                        let deadline_missed = Instant::now() > deadline;
                        metrics_collector.record_packet(priority, latency, deadline_missed);

                        if let Some((socket, target_addr)) = socket_map.get(&priority) {
                            // Non-blocking send; retry on WouldBlock to preserve packet ordering.
                            loop {
                                match socket.send_to(packet.payload(), *target_addr) {
                                    Ok(_) => break,
                                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                        std::thread::yield_now();
                                        continue;
                                    }
                                    Err(_) => break,
                                }
                            }
                        } else if priority == Priority::BestEffort {
                            // Best effort packets without a configured socket are simply dropped
                        }
                    }
                    Err(TryRecvError::Empty) => {
                        idle_cycles += 1;
                        if idle_cycles >= num_priorities {
                            std::thread::yield_now();
                            idle_cycles = 0;
                        }
                    }
                    Err(TryRecvError::Disconnected) => break,
                }
            }
        });

        handle.await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {}
