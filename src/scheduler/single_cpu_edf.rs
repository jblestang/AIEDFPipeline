//! Single-CPU EDF Scheduler with Direct Socket Reading
//!
//! This scheduler implements a single-CPU EDF scheduler that maintains three separate EDF schedulers,
//! one per priority (HIGH, MEDIUM, LOW). The scheduler loops through these EDFs, compares the first
//! deadline of each priority, and executes the one with the earliest deadline. When an EDF is empty
//! during comparison, it attempts to refill it by reading directly from the corresponding priority sockets.
//!
//! Algorithm:
//! 1. Maintain three EDF schedulers (VecDeque) - one per priority (HIGH, MEDIUM, LOW)
//! 2. Loop on those EDF schedulers
//! 3. Compare the first deadline of each priority to find which one to execute
//! 4. When comparing deadlines, if one EDF is empty, try refilling the EDF by reading from
//!    the corresponding socket priority
//! 5. Process the packet with the earliest deadline
//!
//! Note: Uses back-insertion logic (push_back) since data is globally sorted by deadline
//! within the same priority. This allows O(1) insertion and removal operations.

// Import buffer pool for zero-copy packet allocation
use crate::buffer_pool;
// Import packet representation and maximum size constant
use crate::packet::{Packet, MAX_PACKET_SIZE};
// Import priority classes and helper table structure
use crate::priority::{Priority, PriorityTable};
// Import drop counter structure for metrics compatibility
use crate::scheduler::edf::EDFDropCounters;
// Import lock-free channels for inter-thread communication
use crossbeam_channel::Sender;
// Import VecDeque for FIFO queue with back insertion
use std::collections::VecDeque;
// Import mutex for protecting shared state
use parking_lot::Mutex;
// Import standard UDP socket (non-blocking I/O)
use std::net::UdpSocket as StdUdpSocket;
#[cfg(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "android",
    target_os = "ios"
))]
use libc;
#[cfg(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "android",
    target_os = "ios"
))]
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Task scheduled in the EDF scheduler, ordered by deadline (earliest first).
///
/// Contains a packet and its absolute deadline (timestamp + latency_budget).
/// The scheduler compares deadlines across all priorities to select the earliest.
///
/// Note: Tasks are stored in VecDeque with back-insertion since data is already
/// sorted by deadline within the same priority.
#[derive(Debug, Clone)]
struct EDFTask {
    /// The packet to process
    packet: Packet,
    /// Absolute deadline: timestamp + latency_budget (when the packet must be completed)
    deadline: Instant,
}

/// Socket configuration for a priority class.
#[derive(Debug, Clone)]
struct SocketConfig {
    /// The UDP socket to read from
    socket: Arc<StdUdpSocket>,
    /// Latency budget for packets from this socket
    latency_budget: Duration,
}

/// Single-CPU EDF scheduler with three priority-based EDF schedulers.
///
/// The scheduler maintains three separate EDF schedulers (one per priority: HIGH, MEDIUM, LOW),
/// compares deadlines across them, and processes the earliest deadline. When an EDF is empty,
/// it attempts to refill it by reading directly from sockets for that priority.
///
/// Uses VecDeque with back-insertion since data is already sorted by deadline within each priority.
pub struct SingleCPUEDFScheduler {
    /// Per-priority EDF schedulers (VecDeque with back-insertion for O(1) operations)
    edf_queues: PriorityTable<Arc<Mutex<VecDeque<EDFTask>>>>,
    /// Per-priority socket configurations (multiple sockets per priority supported)
    socket_configs: Arc<Mutex<PriorityTable<Vec<SocketConfig>>>>,
    /// Per-priority output channels to egress DRR scheduler
    output_queues: PriorityTable<Sender<Packet>>,
    /// Per-priority atomic drop counters (incremented when egress queue is full)
    output_drop_counters: PriorityTable<Arc<AtomicU64>>,
    /// Shutdown flag
    running: Arc<AtomicBool>,
}

impl SingleCPUEDFScheduler {
    /// Construct a new single-CPU EDF scheduler.
    ///
    /// # Arguments
    /// * `output_queues` - Per-priority output channels to egress DRR
    pub fn new(
        output_queues: PriorityTable<Sender<Packet>>,
    ) -> Self {
        // Initialize drop counters to zero for each priority
        let output_drop_counters = PriorityTable::from_fn(|_| Arc::new(AtomicU64::new(0)));
        // Initialize EDF queues (VecDeque for each priority with back-insertion)
        let edf_queues = PriorityTable::from_fn(|_| Arc::new(Mutex::new(VecDeque::new())));
        // Initialize socket configs (empty initially)
        let socket_configs = Arc::new(Mutex::new(PriorityTable::from_fn(|_| Vec::new())));
        
        Self {
            edf_queues,
            socket_configs,
            output_queues,
            output_drop_counters,
            running: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Register a new input socket associated with a priority class.
    ///
    /// Multiple sockets can be registered for the same priority. When refilling an EDF,
    /// all sockets of the same priority are polled together.
    ///
    /// # Arguments
    /// * `socket` - The UDP socket to read packets from
    /// * `priority` - The priority class for packets from this socket
    /// * `latency_budget` - Maximum time allowed for packets to traverse the pipeline
    pub fn add_socket(
        &self,
        socket: Arc<StdUdpSocket>,
        priority: Priority,
        latency_budget: Duration,
    ) {
        let config = SocketConfig {
            socket,
            latency_budget,
        };
        let mut configs = self.socket_configs.lock();
        configs[priority].push(config);
    }

    /// Return the currently recorded drop counts (used by metrics).
    ///
    /// Reads the atomic drop counters for each priority and returns them in a snapshot.
    pub fn get_drop_counts(&self) -> EDFDropCounters {
        EDFDropCounters {
            heap: 0, // Always 0 (no heap in this implementation)
            // Read each drop counter atomically (relaxed ordering is sufficient for metrics)
            output: PriorityTable::from_fn(|priority| {
                self.output_drop_counters[priority].load(AtomicOrdering::Relaxed)
            }),
        }
    }

    /// Refill an EDF queue by reading from sockets for the given priority.
    ///
    /// Attempts to read packets from all sockets associated with the priority and add them
    /// to the EDF queue. Returns the number of packets read.
    ///
    /// # Arguments
    /// * `priority` - The priority class to refill
    ///
    /// # Returns
    /// Number of packets read and added to the EDF queue
    fn refill_edf(&self, priority: Priority) -> usize {
        let socket_configs = self.socket_configs.lock();
        let sockets = &socket_configs[priority];
        
        if sockets.is_empty() {
            return 0; // No sockets registered for this priority
        }

        let mut packets_read = 0;
        let edf_queue = self.edf_queues[priority].clone();

        // Try to read from all sockets of this priority
        for socket_config in sockets.iter() {
            if !self.running.load(AtomicOrdering::Relaxed) {
                break; // Shutdown requested
            }

            let socket = &socket_config.socket;
            let latency_budget = socket_config.latency_budget;

            // Check if socket has data available
            let size_hint = datagram_size_hint(socket);
            if size_hint == 0 {
                continue; // No data available, try next socket
            }

            // Non-blocking read from UDP socket using a pooled buffer
            let expected = size_hint;
            let mut lease = buffer_pool::lease(expected);
            
            match socket.recv_from(lease.as_mut_slice()) {
                Ok((size, _addr)) => {
                    if size > 0 {
                        // Successfully received a packet
                        let buffer = lease.freeze(size);
                        let packet = Packet::from_buffer(priority, buffer, latency_budget);
                        
                        // Calculate absolute deadline: timestamp + latency budget
                        let deadline = packet.timestamp + packet.latency_budget;
                        let task = EDFTask { packet, deadline };
                        
                        // Add to EDF queue using back-insertion (data is already sorted by deadline)
                        edf_queue.lock().push_back(task);
                        packets_read += 1;
                    } else {
                        // Zero-length packet (unusual but possible), skip it
                        drop(lease); // Return buffer to pool
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // No data for this socket yet (socket is empty)
                    drop(lease); // Return buffer to pool
                    continue; // Try next socket
                }
                Err(_) => {
                    // Socket error; skip to next socket
                    drop(lease); // Return buffer to pool
                    continue;
                }
            }
        }

        packets_read
    }

    /// Process the next available packet (if any).
    ///
    /// Algorithm:
    /// 1. For each priority (HIGH, MEDIUM, LOW), check if EDF is empty and try to refill it
    /// 2. Compare the first deadline of each priority to find which one to execute
    /// 3. Process the packet with the earliest deadline
    /// 4. Forward to egress DRR (or drop if queue full)
    ///
    /// # Returns
    /// `true` if a packet was processed, `false` if no packets were available
    pub fn process_next(&self) -> bool {
        // Priorities to process (HIGH, MEDIUM, LOW only)
        let priorities = [Priority::High, Priority::Medium, Priority::Low];

        // ========================================================================
        // STEP 1: Refill empty EDF queues by reading from sockets
        // ========================================================================
        // For each priority, if the EDF is empty, try to refill it from sockets
        for priority in priorities.iter() {
            let edf_queue = &self.edf_queues[*priority];
            let is_empty = edf_queue.lock().is_empty();
            
            if is_empty {
                // EDF is empty, try to refill it from sockets
                self.refill_edf(*priority);
            }
        }

        // ========================================================================
        // STEP 2: Compare the first deadline of each priority to find which one to execute
        // ========================================================================
        // Find the earliest deadline among all three EDF queues
        let mut earliest: Option<(Priority, Instant)> = None;
        
        for priority in priorities.iter() {
            let edf_queue = &self.edf_queues[*priority];
            let queue_guard = edf_queue.lock();
            
            // Check if this EDF has a packet (front of the queue has earliest deadline)
            if let Some(task) = queue_guard.front() {
                match earliest {
                    // No earliest found yet: this is the first packet we've seen
                    None => earliest = Some((*priority, task.deadline)),
                    // We have a candidate: compare deadlines
                    Some((_, current_deadline)) if task.deadline < current_deadline => {
                        // This packet has an earlier deadline: it becomes the new candidate
                        earliest = Some((*priority, task.deadline));
                    }
                    // This packet has a later deadline: keep the current candidate
                    _ => {}
                }
            }
        }

        // Check if we found any packet to process
        let Some((selected_priority, _)) = earliest else {
            // No packets available: all EDFs are empty
            return false;
        };

        // ========================================================================
        // STEP 3: Remove the selected packet from the EDF queue (front of queue)
        // ========================================================================
        let edf_queue = &self.edf_queues[selected_priority];
        let task = {
            let mut queue_guard = edf_queue.lock();
            queue_guard.pop_front().expect("task must exist for selected priority")
        };

        // Extract the packet from the task
        let packet = task.packet;

        // ========================================================================
        // STEP 4: Simulate packet processing (busy-wait)
        // ========================================================================
        // Deterministic processing time based on payload size.
        // Base: 0.10 ms for small packets (<= 200 bytes)
        // Extra: linear scaling from 0 to 0.1 ms for packets between 200 and 1500 bytes
        // Maximum: ~0.20 ms for MTU-sized packets (1500 bytes)

        let base_ms = 0.10; // Base processing time (0.10 ms)
        // Calculate extra processing time for larger packets
        let extra_ms = if packet.len() > 200 {
            // Clamp packet size to MTU (1500 bytes) to avoid unrealistic values
            let clamped = packet.len().min(1500);
            // Linear interpolation: 0 ms extra at 200 bytes, 0.1 ms extra at 1500 bytes
            // Formula: (size - 200) / (1500 - 200) * 0.1
            0.1 * ((clamped - 200) as f64 / 1300.0)
        } else {
            // No extra time for packets <= 200 bytes
            0.0
        };

        // Total processing time in milliseconds
        let processing_time_ms = base_ms + extra_ms;
        // Convert to Duration (divide by 1000 to get seconds)
        let processing_time = Duration::from_secs_f64(processing_time_ms / 1000.0);

        // Busy-wait loop: spin until processing time has elapsed
        let start = Instant::now();
        while start.elapsed() < processing_time {
            std::hint::spin_loop(); // CPU hint: indicates tight spin loop
        }

        // ========================================================================
        // STEP 5: Forward packet to egress DRR (or drop if queue full)
        // ========================================================================
        // Get the priority of the processed packet
        let priority = packet.priority;
        // Try to send to the egress DRR queue (non-blocking)
        if self.output_queues[priority].try_send(packet).is_err() {
            // Send failed (queue full): increment drop counter atomically
            // Relaxed ordering is sufficient: this is just a metric, not synchronization
            self.output_drop_counters[priority].fetch_add(1, AtomicOrdering::Relaxed);
        }

        true
    }

    /// Run the scheduler in a loop until shutdown is requested.
    ///
    /// Continuously processes packets by comparing deadlines across the three EDF schedulers
    /// and executing the earliest deadline. When EDFs are empty, they are refilled from sockets.
    ///
    /// # Arguments
    /// * `running` - Shared atomic flag that signals when to stop (set to false to shutdown)
    pub fn run(&self, running: Arc<AtomicBool>) {
        // Update internal running flag (using interior mutability via Arc)
        // Since running is already Arc<AtomicBool>, we can just use the passed parameter
        // The internal self.running is kept for compatibility but we use the passed parameter
        
        // Main processing loop
        while running.load(AtomicOrdering::Relaxed) {
            // Process next packet (returns false if no packets available)
            if !self.process_next() {
                // No packets available: yield to avoid busy-waiting
                std::thread::yield_now();
            }
        }
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
    Ok(MAX_PACKET_SIZE)
}

/// Get a hint for the size of the next datagram on the socket.
///
/// Uses platform-specific ioctl (FIONREAD) if available, otherwise returns MAX_PACKET_SIZE.
///
/// # Arguments
/// * `socket` - The UDP socket to query
///
/// # Returns
/// Estimated datagram size, or MAX_PACKET_SIZE if unavailable
fn datagram_size_hint(socket: &StdUdpSocket) -> usize {
    match socket_bytes_available(socket) {
        Ok(available) if available > 0 => available, // Use actual size if available
        _ => MAX_PACKET_SIZE,                        // Fallback to maximum size
    }
}

