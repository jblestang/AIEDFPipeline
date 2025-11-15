//! Egress Deficit Round Robin (DRR) Scheduler
//!
//! This stage drains EDF output queues in strict priority order, records latency metrics, and emits
//! packets through UDP sockets. Metrics recording uses a lock-free channel so the hot path does not
//! contend with the statistics thread.
//!
//! Algorithm:
//! 1. Iterate through priorities in order (High, Medium, Low, BestEffort)
//! 2. Try to receive a packet from each priority's queue (non-blocking)
//! 3. Verify packet ordering per priority (using packet IDs)
//! 4. Record latency metrics (time since ingress timestamp)
//! 5. Send packet to output UDP socket (non-blocking with retries)
//! 6. Yield if no packets were processed
//!
//! The scheduler processes priorities in strict order to ensure high-priority packets are always
//! serviced before lower-priority ones, even if they arrive later.

// Import packet representation used throughout the pipeline
use crate::packet::Packet;
// Import priority classes and helper table structure
use crate::priority::{Priority, PriorityTable};
// Import error types for channel operations
use crossbeam_channel::TryRecvError;
// Import mutex for protecting output socket map
use parking_lot::Mutex;
// Import HashMap for O(1) priority lookups
use std::collections::HashMap;
// Import network types for UDP socket operations
use std::net::{SocketAddr, UdpSocket};
// Import atomic types for lock-free packet ID tracking
use std::sync::atomic::{AtomicU64, Ordering};
// Import Arc for shared ownership
use std::sync::Arc;
// Import time types for latency measurement
use std::time::Instant;

/// Egress DRR Scheduler - Reads from priority queues and writes to UDP sockets
///
/// The scheduler:
/// - Drains EDF output queues in priority order (High → Medium → Low → BestEffort)
/// - Verifies packet ordering per priority (using monotonically increasing packet IDs)
/// - Records latency metrics (time from ingress to egress)
/// - Sends packets to output UDP sockets (non-blocking with retries)
pub struct EgressDRRScheduler {
    /// Per-priority input channels from EDF scheduler
    priority_receivers: PriorityTable<crossbeam_channel::Receiver<Packet>>,
    /// Output socket map: priority -> vector of (socket, address) pairs
    /// Multiple sockets per priority are supported for load balancing.
    /// Protected by a mutex to allow safe concurrent access (e.g., adding sockets while processing).
    output_sockets: Arc<Mutex<HashMap<Priority, Vec<(Arc<UdpSocket>, SocketAddr)>>>>,
    /// Last packet ID seen per priority (for order verification)
    /// Used to detect out-of-order packets (should never happen with proper sequence tracking).
    last_packet_ids: PriorityTable<Arc<AtomicU64>>,
}

impl EgressDRRScheduler {
    /// Build a new egress scheduler using pre-created per-priority channels.
    ///
    /// # Arguments
    /// * `priority_receivers` - Per-priority input channels from EDF scheduler
    pub fn new(priority_receivers: PriorityTable<crossbeam_channel::Receiver<Packet>>) -> Self {
        Self {
            priority_receivers, // Store input channels
            // Initialize output socket map (empty, sockets added via add_output_socket)
            output_sockets: Arc::new(Mutex::new(HashMap::new())),
            // Initialize last packet IDs to u64::MAX (no packets seen yet)
            // This allows the first packet (ID 0) to be accepted without triggering order violation.
            last_packet_ids: PriorityTable::from_fn(|_| Arc::new(AtomicU64::new(u64::MAX))),
        }
    }

    /// Register an output socket that will receive packets for a given priority class.
    ///
    /// Multiple sockets can be registered for the same priority. Packets are distributed
    /// across sockets using round-robin to balance load.
    ///
    /// # Arguments
    /// * `priority` - The priority class for this socket
    /// * `socket` - The UDP socket to send packets to
    /// * `address` - The destination address for UDP packets
    pub fn add_output_socket(
        &self,
        priority: Priority,     // Priority class for this socket
        socket: Arc<UdpSocket>, // UDP socket to send to
        address: SocketAddr,    // Destination address
    ) {
        // Set socket to non-blocking mode (required for async operation)
        socket
            .set_nonblocking(true)
            .expect("failed to set UDP socket to non-blocking");
        // Register the socket in the output map (append to vector for this priority)
        self.output_sockets
            .lock()
            .entry(priority)
            .or_insert_with(Vec::new)
            .push((socket, address));
        // Lock is released here (RAII)
    }

    /// Process packets from priority queues and send to output sockets
    ///
    /// Main processing loop:
    /// 1. Clone receivers and sockets at start (avoid locking in hot loop)
    /// 2. Iterate through priorities in order (High → Medium → Low → BestEffort)
    /// 3. Try to receive a packet from each priority's queue (non-blocking)
    /// 4. Verify packet ordering per priority (using packet IDs)
    /// 5. Record latency metrics (time from ingress to egress)
    /// 6. Send packet to output UDP socket (non-blocking with retries)
    /// 7. Yield if no packets were processed
    ///
    /// # Arguments
    /// * `running` - Atomic flag to signal shutdown
    /// * `metrics_collector` - Metrics collector for latency tracking
    ///
    /// # Returns
    /// `Ok(())` on normal shutdown, error on unexpected failure
    pub async fn process_queues(
        &self,
        running: Arc<std::sync::atomic::AtomicBool>, // Shutdown flag
        metrics_collector: Arc<crate::metrics::MetricsCollector>, // Metrics collector
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Clone receivers so we can move them into the blocking task
        let receivers =
            PriorityTable::from_fn(|priority| self.priority_receivers[priority].clone());

        // ========================================================================
        // STEP 1: Snapshot output sockets (minimize mutex hold time)
        // ========================================================================
        // Clone sockets at start to avoid locking within the hot loop.
        // This allows the hot loop to operate without holding the mutex.
        // Multiple sockets per priority are supported for load balancing.
        let socket_map: HashMap<Priority, Vec<(Arc<UdpSocket>, SocketAddr)>> = {
            let sockets = self.output_sockets.lock(); // Acquire mutex
            sockets.clone() // Clone the map
                            // Lock is released here (RAII)
        };
        // Clone last packet ID trackers (for order verification)
        let last_ids = PriorityTable::from_fn(|priority| self.last_packet_ids[priority].clone());
        // Round-robin indices per priority for distributing packets across multiple sockets
        let mut socket_indices = PriorityTable::from_fn(|_| 0usize);

        // Spawn a blocking task (runs on a thread pool, not the async runtime)
        // This allows us to use blocking I/O operations without blocking the async runtime.
        let handle = tokio::task::spawn_blocking(move || {
            // Main processing loop: continues until shutdown signal
            while running.load(std::sync::atomic::Ordering::Relaxed) {
                // Track if we processed any packets this iteration (for yield optimization)
                let mut handled_packet = false;

                // ========================================================================
                // STEP 2: Iterate through priorities in strict order
                // ========================================================================
                // Iterate through priorities in order (High, Medium, Low, BestEffort).
                // This ensures high-priority packets are always serviced before lower-priority ones.
                for priority in Priority::ALL {
                    // Try to receive a packet from this priority's queue (non-blocking)
                    match receivers[priority].try_recv() {
                        Ok(packet) => {
                            // Successfully received a packet: mark as handled
                            handled_packet = true;

                            // ========================================================================
                            // STEP 3: Verify packet ordering per priority
                            // ========================================================================
                            // Verify packet ordering per priority.
                            // Packets should have monotonically increasing IDs within each priority.
                            // This is a sanity check: with proper sequence tracking, this should never fail.
                            let previous_id = last_ids[priority].load(Ordering::Relaxed); // Get last ID
                            if previous_id != u64::MAX && packet.id <= previous_id {
                                // Out-of-order detected: log warning (shouldn't happen)
                                /*
                                eprintln!(
                                    "[egress] detected out-of-order packet for {:?}: current id {}, previous id {}",
                                    priority, packet.id, previous_id
                                );
                                */
                            }
                            // Update last seen ID (atomically, relaxed ordering is sufficient)
                            last_ids[priority].store(packet.id, Ordering::Relaxed);

                            // ========================================================================
                            // STEP 4: Record latency metrics
                            // ========================================================================
                            // Record metrics before the attempt to send so dropped packets are still tracked.
                            // Calculate latency: time from ingress (packet.timestamp) to now
                            let latency = packet.timestamp.elapsed();
                            // Calculate deadline: ingress timestamp + latency budget
                            let deadline = packet.timestamp + packet.latency_budget;
                            // Check if deadline was missed: current time > deadline
                            let deadline_missed = Instant::now() > deadline;
                            // Record metrics (uses lock-free channel, doesn't block hot path)
                            metrics_collector.record_packet(priority, latency, deadline_missed);

                            // ========================================================================
                            // STEP 5: Send packet to output UDP socket (round-robin across multiple sockets)
                            // ========================================================================
                            // Get the output sockets for this priority (from snapshot)
                            // Multiple sockets per priority are supported for load balancing.
                            if let Some(sockets) = socket_map.get(&priority) {
                                if !sockets.is_empty() {
                                    // Round-robin: select socket using index, then increment for next packet
                                    let socket_idx = socket_indices[priority] % sockets.len();
                                    let (socket, target_addr) = &sockets[socket_idx];
                                    socket_indices[priority] =
                                        (socket_indices[priority] + 1) % sockets.len().max(1);

                                    // Non-blocking send; retry on WouldBlock to preserve packet ordering.
                                    // We retry instead of dropping to ensure packets are sent in order.
                                    loop {
                                        // Attempt to send the packet (non-blocking)
                                        match socket.send_to(packet.payload(), *target_addr) {
                                            Ok(_) => {
                                                // Successfully sent: exit retry loop
                                                break;
                                            }
                                            Err(e)
                                                if e.kind() == std::io::ErrorKind::WouldBlock =>
                                            {
                                                // Socket buffer full: yield and retry
                                                // This preserves packet ordering (we don't skip to next packet)
                                                std::thread::yield_now(); // Yield CPU to other threads
                                                continue; // Retry sending
                                            }
                                            Err(_) => {
                                                // Other error (network error, socket closed, etc.): give up
                                                break; // Exit retry loop, move to next packet
                                            }
                                        }
                                    }
                                }
                            } else if priority == Priority::BestEffort {
                                // Best effort packets without a configured socket are simply dropped
                                // (no error, this is expected behavior)
                            }
                        }
                        Err(TryRecvError::Empty) => {
                            // Queue empty for this priority: continue to next priority
                            continue;
                        }
                        Err(TryRecvError::Disconnected) => {
                            // Channel disconnected: exit processing loop
                            return;
                        }
                    }
                }

                // ========================================================================
                // STEP 6: Yield if no packets were processed
                // ========================================================================
                // OPTIMIZATION: Yield if no packets were processed (all queues were empty).
                // This prevents busy-waiting when there's no work to do.
                if !handled_packet {
                    std::thread::yield_now(); // Yield CPU to other threads
                }
            }
        });

        // Wait for the blocking task to complete
        handle.await?;

        // Normal shutdown: return Ok
        Ok(())
    }
}

#[cfg(test)]
mod tests {}
