use crate::drr_scheduler::{Packet, Priority, PriorityTable};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::net::UdpSocket;

/// Egress DRR Scheduler - Reads from priority queues and writes to UDP sockets
pub struct EgressDRRScheduler {
    priority_receivers: PriorityTable<crossbeam_channel::Receiver<Packet>>,
    // Output socket map: priority -> (socket, address)
    output_sockets: Arc<Mutex<HashMap<Priority, (Arc<UdpSocket>, SocketAddr)>>>,
}

impl EgressDRRScheduler {
    pub fn new(priority_receivers: PriorityTable<crossbeam_channel::Receiver<Packet>>) -> Self {
        Self {
            priority_receivers,
            output_sockets: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn add_output_socket(
        &self,
        priority: Priority,
        socket: Arc<UdpSocket>,
        address: SocketAddr,
    ) {
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

        // OPTIMIZATION: Clone socket map once at start to avoid lock per packet
        // NOTE: This means sockets added after process_queues starts won't be available
        // until the next iteration. For now, we assume all sockets are added before
        // process_queues is called.
        // IMPORTANT: Clone socket map AFTER cloning receivers to ensure all sockets are added
        let socket_map: HashMap<Priority, (Arc<UdpSocket>, SocketAddr)> = {
            let sockets = self.output_sockets.lock();
            sockets.clone()
        };

        tokio::spawn(async move {
            while running.load(std::sync::atomic::Ordering::Relaxed) {
                let mut packet_opt = None;
                for priority in Priority::ALL {
                    if let Ok(packet) = receivers[priority].try_recv() {
                        packet_opt = Some((priority, packet));
                        break;
                    }
                }

                if let Some((priority, packet)) = packet_opt {
                    // Record metrics
                    let latency = packet.timestamp.elapsed();
                    let deadline = packet.timestamp + packet.latency_budget;
                    let deadline_missed = Instant::now() > deadline;
                    metrics_collector.record_packet(
                        priority,
                        latency,
                        deadline_missed,
                        packet.latency_budget,
                    );

                    if let Some((socket, target_addr)) = socket_map.get(&priority) {
                        let _ = socket.send_to(&packet.data, *target_addr).await;
                    } else if priority == Priority::BestEffort {
                        // Best effort packets without a configured socket are simply dropped
                    }
                } else {
                    // No packets available - yield to other tasks
                    tokio::task::yield_now().await;
                }
            }
        })
        .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {}
