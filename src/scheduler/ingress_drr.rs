//! Ingress Deficit Round Robin (DRR) Scheduler
//!
//! A single Tokio runtime polls non-blocking UDP sockets, applies packet-count based DRR, and
//! forwards packets into per-priority bounded channels destined for the EDF processor.
//!
//! Algorithm:
//! 1. Maintain per-priority deficit counters (initialized to 0)
//! 2. Round-robin through active flows (priorities with registered sockets)
//! 3. For each flow: add quantum to deficit, then read packets until deficit is exhausted
//! 4. Forward packets to per-priority EDF input queues (or drop if queue full)
//! 5. Carry remaining deficit to next round (ensures fairness over time)
//!
//! The DRR ensures fair packet distribution across priorities while prioritizing high-priority flows
//! through larger quanta. Packet-count based means we count packets, not bytes.

// Import buffer pool for zero-copy packet allocation
use crate::buffer_pool;
// Import packet representation and maximum size constant
use crate::packet::{Packet, MAX_PACKET_SIZE};
// Import priority classes and helper table structure
use crate::priority::{Priority, PriorityTable};
// Import mutex for protecting shared state
use parking_lot::Mutex;
// Import HashMap for O(1) priority lookups
use std::collections::HashMap;
// Import standard UDP socket (non-blocking I/O)
use std::net::UdpSocket as StdUdpSocket;
#[cfg(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "android",
    target_os = "ios"
))]
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// DRR state tracked per priority class.
///
/// Each priority flow maintains:
/// - A quantum (number of packets allowed per round)
/// - A deficit counter (remaining credit from current + previous rounds)
/// - A latency budget (used when creating Packet objects)
#[derive(Debug)]
struct FlowState {
    /// Number of packets the flow can emit in the current round.
    /// Higher priorities typically have larger quanta to ensure they get more service.
    quantum: usize,
    /// Remaining credit within the current round (updated atomically).
    /// When a flow is serviced, quantum is added to deficit. As packets are read,
    /// deficit is decremented. Remaining deficit carries over to the next round.
    deficit: AtomicUsize,
    /// Latency budget used when instantiating [`Packet`] objects.
    /// This is the maximum time allowed for the packet to traverse the pipeline.
    latency_budget: Duration,
}

/// Snapshot used during processing to avoid holding the state lock in the hot loop.
///
/// The main processing loop takes a snapshot of all active flows (sockets + states)
/// at the start of each round. This allows the hot loop to operate without holding
/// the mutex, reducing contention and improving performance.
#[derive(Clone)]
struct FlowSnapshot {
    /// All UDP sockets for this priority flow (multiple sockets per priority supported)
    /// When processing, we poll all sockets together to maximize throughput.
    sockets: Vec<Arc<StdUdpSocket>>,
    /// The DRR state (quantum, deficit, latency_budget) for this priority
    state: Arc<FlowState>,
}

/// Combined state guarded by a single mutex so we can minimise lock churn while iterating flows.
///
/// All mutable state is grouped together and protected by a single mutex. This allows
/// taking a snapshot of the entire state in one lock acquisition, then releasing the
/// lock before entering the hot processing loop.
#[derive(Debug)]
struct IngressDRRState {
    /// Socket configurations keyed by priority for O(1) lookup.
    /// Maps each priority to a vector of UDP sockets (multiple sockets per priority supported).
    /// When processing, all sockets of the same priority are polled together.
    socket_configs: HashMap<Priority, Vec<SocketConfig>>,
    /// Per-priority DRR state (quantum + deficit atomics).
    /// Each priority has its own FlowState with quantum and atomic deficit counter.
    flow_states: HashMap<Priority, Arc<FlowState>>,
    /// Ordered list of active flows used by the round-robin iterator.
    /// Contains priorities that have registered sockets, in the order they should be serviced.
    /// The order is stable (High, Medium, Low, BestEffort) to ensure priority ordering.
    active_flows: Vec<Priority>,
    /// Index within `active_flows` that will be serviced next.
    /// Used for round-robin: we start servicing from this index and wrap around.
    current_flow_index: usize,
}

/// Ingress DRR scheduler that enforces packet-count fairness before handing packets to EDF.
///
/// The scheduler:
/// - Polls UDP sockets in round-robin order
/// - Applies DRR (Deficit Round Robin) to ensure fair packet distribution
/// - Forwards packets to per-priority EDF input queues
/// - Drops packets if EDF queues are full (counted for metrics)
///
/// Packet-count DRR means we count packets, not bytes. Each priority gets a quantum
/// (number of packets) per round, and unused credit (deficit) carries over to the next round.
pub struct IngressDRRScheduler {
    /// Crossbeam senders for each priority class.
    /// These channels forward packets to the EDF scheduler (one channel per priority).
    priority_queues: PriorityTable<crossbeam_channel::Sender<Packet>>,
    /// Shared mutable state for flow metadata.
    /// Protected by a mutex to allow safe concurrent access (e.g., adding sockets while processing).
    state: Arc<Mutex<IngressDRRState>>,
    /// Drop counter per priority exposed via metrics.
    /// Incremented atomically when a packet is dropped (EDF queue full).
    drop_counters: PriorityTable<Arc<AtomicU64>>,
}

/// Configuration captured when adding an input socket.
///
/// Stores the UDP socket associated with a priority class.
#[derive(Debug, Clone)]
pub struct SocketConfig {
    /// The UDP socket to read packets from
    pub socket: Arc<StdUdpSocket>,
}

impl IngressDRRScheduler {
    /// Create a new ingress DRR scheduler bound to the provided priority queues.
    ///
    /// # Arguments
    /// * `priority_queues` - Per-priority output channels to the EDF scheduler
    pub fn new(priority_queues: PriorityTable<crossbeam_channel::Sender<Packet>>) -> Self {
        // Initialize drop counters to zero for each priority
        let drop_counters = PriorityTable::from_fn(|_| Arc::new(AtomicU64::new(0)));
        Self {
            priority_queues, // Store output channels
            // Initialize state: no sockets registered yet, empty active flows list
            state: Arc::new(Mutex::new(IngressDRRState {
                socket_configs: HashMap::new(), // No sockets yet
                flow_states: HashMap::new(), // No flow states yet
                active_flows: Vec::new(), // No active flows yet
                current_flow_index: 0, // Start at index 0 for round-robin
            })),
            drop_counters, // Store atomic drop counters
        }
    }

    /// Return per-priority ingress drop counters for metrics emission.
    ///
    /// Reads the atomic drop counters for each priority and returns them in a snapshot.
    /// This is called periodically by the metrics collector to track packet drops.
    pub fn get_drop_counts(&self) -> PriorityTable<u64> {
        // Read each drop counter atomically (relaxed ordering is sufficient for metrics)
        PriorityTable::from_fn(|priority| self.drop_counters[priority].load(Ordering::Relaxed))
    }

    /// Register a new input socket associated with a priority class and DRR quantum.
    ///
    /// Multiple sockets can be registered for the same priority. When processing,
    /// all sockets of the same priority are polled together to maximize throughput.
    ///
    /// # Arguments
    /// * `socket` - The UDP socket to read packets from
    /// * `priority` - The priority class for packets from this socket
    /// * `latency_budget` - Maximum time allowed for packets to traverse the pipeline
    /// * `quantum` - Number of packets this flow can emit per DRR round
    ///
    /// Higher priorities typically have larger quanta to ensure they get more service.
    pub fn add_socket(
        &self,
        socket: Arc<StdUdpSocket>, // UDP socket to read from
        priority: Priority, // Priority class for this socket
        latency_budget: Duration, // Maximum latency budget for packets
        quantum: usize, // DRR quantum (packets per round)
    ) {
        // Create socket configuration
        let config = SocketConfig { socket };

        // OPTIMISATION: Build per-flow state once and store atomics for deficit updates
        // The deficit counter is atomic, so updates in the hot loop don't require locking.
        // Only create flow state if this is the first socket for this priority.
        let mut state = self.state.lock();
        
        // Get or create flow state for this priority (shared across all sockets of same priority)
        let _flow_state = state.flow_states.entry(priority).or_insert_with(|| {
            Arc::new(FlowState {
                quantum, // Store quantum (number of packets per round)
                deficit: AtomicUsize::new(0), // Initialize deficit to 0 (no credit yet)
                latency_budget, // Store latency budget for packet creation
            })
        });
        
        // Register the socket configuration for this priority (append to vector)
        state.socket_configs.entry(priority).or_insert_with(Vec::new).push(config);
        
        // Add to active flows list if not already present (for round-robin iteration)
        if !state.active_flows.contains(&priority) {
            state.active_flows.push(priority);
        }
        // Lock is released here (RAII)
    }

    /// Drive all sockets with packet-count DRR until shutdown.
    ///
    /// Main processing loop:
    /// 1. Take a snapshot of active flows (sockets + states) while holding the mutex
    /// 2. Release the mutex and iterate through flows in round-robin order
    /// 3. For each flow: add quantum to deficit, then read packets until deficit exhausted
    /// 4. Forward packets to EDF queues (or drop if full)
    /// 5. Update round-robin index and yield if no packets were read
    ///
    /// # Arguments
    /// * `running` - Atomic flag to signal shutdown
    ///
    /// # Returns
    /// `Ok(())` on normal shutdown, error on unexpected failure
    pub async fn process_sockets(
        &self,
        running: Arc<AtomicBool>, // Shutdown flag (checked each iteration)
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Main processing loop: continues until shutdown signal
        while running.load(Ordering::Relaxed) {
            // ========================================================================
            // STEP 1: Snapshot shared state (minimize mutex hold time)
            // ========================================================================
            // Snapshot shared state outside the hot loop so we keep the mutex hold time short.
            // We clone the necessary data structures so the hot loop can operate without the lock.
            let (active_flows, current_index, snapshot_map) = {
                // Acquire the mutex to access shared state
                let state = self.state.lock();
                // Check if there are any active flows
                let is_empty = state.active_flows.is_empty();
                // Clone the active flows list (or create empty vec if none)
                let active_flows = if is_empty {
                    Vec::new() // No active flows
                } else {
                    state.active_flows.clone() // Clone the list
                };
                // Get the current round-robin index
                let current_index = state.current_flow_index;
                // Build a snapshot map: priority -> (sockets, flow_state)
                // This allows O(1) lookup in the hot loop without holding the mutex.
                // All sockets for the same priority are collected together for batch polling.
                let snapshot_map: HashMap<Priority, FlowSnapshot> = state
                    .active_flows
                    .iter()
                    .filter_map(|priority| {
                        // Get socket configs for this priority (may be multiple sockets)
                        let socket_configs = state.socket_configs.get(priority)?;
                        // Get flow state for this priority
                        let flow_state = state.flow_states.get(priority)?;
                        // Collect all sockets for this priority into a vector
                        let sockets: Vec<Arc<StdUdpSocket>> = socket_configs
                            .iter()
                            .map(|config| config.socket.clone())
                            .collect();
                        // Create snapshot entry with all sockets for this priority
                        Some((
                            *priority, // Priority as key
                            FlowSnapshot {
                                sockets, // All sockets for this priority
                                state: flow_state.clone(), // Clone flow state Arc
                            },
                        ))
                    })
                    .collect();
                // Explicitly drop the mutex before any await points (prevents deadlock)
                drop(state);
                // Return the snapshot data
                (active_flows, current_index, snapshot_map)
            };

            if active_flows.is_empty() {
                tokio::task::yield_now().await;
                continue;
            }

            let mut current_index = current_index;
            let mut packets_read = 0;

            // Iterate through flows in round-robin order and honour the per-priority deficit.
            for i in 0..active_flows.len() {
                let flow_index = (current_index + i) % active_flows.len();
                if !running.load(Ordering::Relaxed) {
                    break;
                }

                let priority = active_flows[flow_index];

                // Get sockets and flow state from snapshot (no lock needed)
                let flow_snapshot = match snapshot_map.get(&priority) {
                    Some(snapshot) => snapshot.clone(),
                    None => continue,
                };
                let sockets = &flow_snapshot.sockets;
                let flow_state = flow_snapshot.state;

                // Atomically add quantum to deficit and retrieve local copy
                let mut local_deficit = flow_state
                    .deficit
                    .fetch_add(flow_state.quantum, Ordering::Relaxed)
                    + flow_state.quantum;

                if local_deficit == 0 {
                    continue;
                }

                // ========================================================================
                // STEP 4: Read packets from all sockets of this priority until deficit is exhausted
                // ========================================================================
                // Poll all sockets of the same priority together to maximize throughput.
                // This ensures we drain all available packets from all sockets before moving
                // to the next priority, improving fairness and reducing latency.
                // Inner loop: read packets from all sockets of this priority until:
                // - Deficit is exhausted (no more credit)
                // - All sockets are empty (WouldBlock errors)
                // - Shutdown is requested
                // - EDF queue is full (drop packet and move to next flow)
                loop {
                    // Check shutdown flag
                    if !running.load(Ordering::Relaxed) {
                        break; // Shutdown requested, exit inner loop
                    }

                    // Check if we've exhausted our deficit (no more credit)
                    if local_deficit == 0 {
                        break; // No more credit, move to next flow
                    }

                    // Track if we read any packets from any socket in this iteration
                    let mut read_any = false;

                    // Poll all sockets of this priority together
                    for socket in sockets {
                        // Check if we've exhausted our deficit (may have been consumed by previous sockets)
                        if local_deficit == 0 {
                            break; // No more credit, exit socket loop
                        }

                        // Check shutdown flag
                        if !running.load(Ordering::Relaxed) {
                            break; // Shutdown requested, exit socket loop
                        }

                        // Skip to next socket if this one has no data (optimization)
                        let size_hint = datagram_size_hint(socket);
                        if size_hint == 0 {
                            continue; // No data available, try next socket
                        }

                        // Non-blocking read from UDP socket using a pooled buffer sized to the datagram.
                        // We use FIONREAD (if available) to get the expected datagram size, allowing
                        // us to allocate the right-sized buffer from the pool (zero-copy optimization).
                        let expected = size_hint;
                        let mut lease = buffer_pool::lease(expected); // Lease buffer from pool
                        // Attempt to receive a UDP datagram (non-blocking)
                        match socket.recv_from(lease.as_mut_slice()) {
                            Ok((size, _addr)) if size > 0 => {
                                // Successfully received a packet: decrement deficit
                                local_deficit = local_deficit.saturating_sub(1);
                                read_any = true;

                                // Materialise packet (including timestamp used for EDF deadlines).
                                // The buffer is frozen at the actual size (may be smaller than expected).
                                let buffer = lease.freeze(size); // Freeze buffer at actual size
                                // Create packet with priority, buffer, and latency budget
                                let packet =
                                    Packet::from_buffer(priority, buffer, flow_state.latency_budget);

                                // Get the output channel for this priority
                                let tx = &self.priority_queues[priority];

                                // Try to send to EDF queue (non-blocking)
                                if tx.try_send(packet).is_ok() {
                                    // Successfully forwarded: increment packet counter
                                    packets_read += 1;
                                } else {
                                    // Channel full → drop packet and move to the next flow.
                                    // Increment drop counter atomically (relaxed ordering is sufficient)
                                    self.drop_counters[priority].fetch_add(1, Ordering::Relaxed);
                                    break; // Exit socket loop, will exit inner loop
                                }
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                // No data for this socket yet (socket is empty).
                                // This is expected: the socket is non-blocking and has no data.
                                drop(lease); // Return buffer to pool
                                continue; // Try next socket
                            }
                            Err(_) => {
                                // Socket error; skip to next socket.
                                // This could be a network error, socket closed, etc.
                                drop(lease); // Return buffer to pool
                                continue; // Try next socket
                            }
                            _ => {
                                // Unexpected zero-byte packet or error, skip to next socket.
                                // This shouldn't happen normally, but we handle it gracefully.
                                continue; // Try next socket
                            }
                        }
                    }

                    // If we didn't read any packets from any socket, all sockets are empty
                    if !read_any {
                        break; // All sockets empty, exit inner loop, move to next flow
                    }
                }

                // ========================================================================
                // STEP 5: Persist updated deficit for the next round
                // ========================================================================
                // Persist updated deficit for the next round (no lock required).
                // The deficit counter is atomic, so we can update it without holding the mutex.
                // Remaining deficit carries over to the next round (ensures fairness over time).
                flow_state.deficit.store(local_deficit, Ordering::Relaxed);
            }

            // ========================================================================
            // STEP 6: Update round-robin index and yield if no work done
            // ========================================================================
            // Advance the round-robin pointer once we serviced everyone.
            // We increment the index so the next round starts from the next flow.
            current_index = (current_index + 1) % active_flows.len();

            // Persist the updated index (requires mutex, but brief)
            self.state.lock().current_flow_index = current_index;

            // OPTIMIZATION: Yield if no packets were read (all sockets were empty).
            // This prevents busy-waiting when there's no work to do.
            if packets_read == 0 {
                tokio::task::yield_now().await; // Yield to other tasks
            }
        }

        // Normal shutdown: return Ok
        Ok(())
    }
}

/// Get a hint for the expected datagram size on the socket.
///
/// Uses FIONREAD (if available) to query the socket for pending data size.
/// This allows allocating the right-sized buffer from the pool (zero-copy optimization).
/// Falls back to MAX_PACKET_SIZE if the query fails or is unavailable.
///
/// # Arguments
/// * `socket` - The UDP socket to query
///
/// # Returns
/// Expected datagram size (or MAX_PACKET_SIZE if unknown)
fn datagram_size_hint(socket: &StdUdpSocket) -> usize {
    match socket_bytes_available(socket) {
        Ok(available) if available > 0 => available, // Use actual size if available
        _ => MAX_PACKET_SIZE, // Fallback to maximum size
    }
}

/// Query the socket for the number of bytes available to read (platform-specific).
///
/// Uses FIONREAD ioctl on Unix-like systems (macOS, Linux, Android, iOS) to get
/// the size of the next datagram without actually reading it. This allows optimal
/// buffer allocation from the pool.
///
/// # Arguments
/// * `socket` - The UDP socket to query
///
/// # Returns
/// Number of bytes available, or error if the query fails
#[cfg(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "android",
    target_os = "ios"
))]
fn socket_bytes_available(socket: &StdUdpSocket) -> std::io::Result<usize> {
    // Get the raw file descriptor
    let fd = socket.as_raw_fd();
    // Variable to receive the byte count from ioctl
    let mut bytes: libc::c_int = 0;
    // Call FIONREAD ioctl to query available bytes (non-blocking)
    let ret = unsafe { libc::ioctl(fd, libc::FIONREAD, &mut bytes) };
    if ret == -1 {
        // ioctl failed: return the last OS error
        Err(std::io::Error::last_os_error())
    } else {
        // Success: return the byte count (ensure non-negative)
        Ok(bytes.max(0) as usize)
    }
}

/// Fallback for platforms that don't support FIONREAD.
///
/// Returns MAX_PACKET_SIZE as a safe default (we'll allocate a full-size buffer).
///
/// # Arguments
/// * `_socket` - The UDP socket (unused on unsupported platforms)
///
/// # Returns
/// MAX_PACKET_SIZE (safe default)
#[cfg(not(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "android",
    target_os = "ios"
)))]
fn socket_bytes_available(_socket: &StdUdpSocket) -> std::io::Result<usize> {
    // Platform doesn't support FIONREAD: return maximum size
    Ok(MAX_PACKET_SIZE)
}

#[cfg(test)]
mod tests {}
