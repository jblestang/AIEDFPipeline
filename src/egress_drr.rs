use crate::drr_scheduler::Packet;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::net::UdpSocket;

/// Egress DRR Scheduler - Reads from priority queues and writes to UDP sockets
pub struct EgressDRRScheduler {
    // Three priority queues from EDF output
    high_priority_rx: crossbeam_channel::Receiver<Packet>,
    medium_priority_rx: crossbeam_channel::Receiver<Packet>,
    low_priority_rx: crossbeam_channel::Receiver<Packet>,
    // Output socket map: flow_id -> (socket, address)
    output_sockets: Arc<Mutex<HashMap<u64, (Arc<UdpSocket>, SocketAddr)>>>,
}

impl EgressDRRScheduler {
    pub fn new(
        high_priority_rx: crossbeam_channel::Receiver<Packet>,
        medium_priority_rx: crossbeam_channel::Receiver<Packet>,
        low_priority_rx: crossbeam_channel::Receiver<Packet>,
    ) -> Self {
        Self {
            high_priority_rx,
            medium_priority_rx,
            low_priority_rx,
            output_sockets: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn add_output_socket(&self, flow_id: u64, socket: Arc<UdpSocket>, address: SocketAddr) {
        self.output_sockets
            .lock()
            .insert(flow_id, (socket, address));
    }

    /// Process packets from priority queues and send to output sockets
    pub async fn process_queues(
        &self,
        running: Arc<std::sync::atomic::AtomicBool>,
        metrics_collector: Arc<crate::metrics::MetricsCollector>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // OPTIMIZATION: Clone socket map once at start to avoid lock per packet
        let socket_map: HashMap<u64, (Arc<UdpSocket>, SocketAddr)> = {
            let sockets = self.output_sockets.lock();
            sockets.clone()
        };
        
        let high_rx = self.high_priority_rx.clone();
        let medium_rx = self.medium_priority_rx.clone();
        let low_rx = self.low_priority_rx.clone();

        tokio::spawn(async move {
            while running.load(std::sync::atomic::Ordering::Relaxed) {
                // Process in priority order: HIGH -> MEDIUM -> LOW
                let packet_opt = high_rx
                    .try_recv()
                    .ok()
                    .or_else(|| medium_rx.try_recv().ok())
                    .or_else(|| low_rx.try_recv().ok());

                if let Some(packet) = packet_opt {
                    // Record metrics
                    let latency = packet.timestamp.elapsed();
                    let deadline = packet.timestamp + packet.latency_budget;
                    let deadline_missed = Instant::now() > deadline;
                    metrics_collector.record_packet(
                        packet.flow_id,
                        latency,
                        deadline_missed,
                        packet.latency_budget,
                    );

                    // Route to appropriate output socket based on flow_id (no lock needed)
                    if let Some((socket, target_addr)) = socket_map.get(&packet.flow_id) {
                        let _ = socket.send_to(&packet.data, *target_addr).await;
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
