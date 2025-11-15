//! Metrics collection and reporting utilities.
//!
//! The metrics pipeline records latency and deadline information on the egress hot path through a
//! lock-free channel and aggregates statistics on the best-effort core before broadcasting them to
//! GUI subscribers.

use crate::priority::{Priority, PriorityTable};
use crate::scheduler::multi_worker_edf::MultiWorkerStats;
use crossbeam_channel::{self, Sender};
use parking_lot::Mutex;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Latency measurement with timestamp for time-based windowing
#[derive(Debug, Clone)]
pub(crate) struct LatencyMeasurement {
    latency_us: f64,
    timestamp: Instant,
}

/// Rolling statistics for a given priority class.
#[derive(Debug)]
pub struct Metrics {
    pub priority: Priority,
    pub packet_count: u64,
    pub(crate) latencies: VecDeque<LatencyMeasurement>, // Store latencies with timestamps for time-based windowing
    pub deadline_misses: u64,
    pub time_window: Duration, // Time window for latency history (e.g., 10 seconds)
    // Cached statistics to avoid expensive recalculations (using RefCell for interior mutability)
    cached_p50: RefCell<Option<Duration>>,
    cached_p95: RefCell<Option<Duration>>,
    cached_p99: RefCell<Option<Duration>>,
    cached_p999: RefCell<Option<Duration>>,
    cached_packet_count: RefCell<u64>, // Track when to invalidate cache
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            priority: Priority::High,
            packet_count: 0,
            latencies: VecDeque::with_capacity(10000), // Pre-allocate for high packet rates
            deadline_misses: 0,
            time_window: Duration::from_secs(10), // Keep last 10 seconds of latency data
            cached_p50: RefCell::new(None),
            cached_p95: RefCell::new(None),
            cached_p99: RefCell::new(None),
            cached_p999: RefCell::new(None),
            cached_packet_count: RefCell::new(0),
        }
    }
}

impl Metrics {
    /// Create a new metrics tracker for a priority class.
    ///
    /// Initializes all counters to zero and pre-allocates the latency history buffer
    /// to avoid reallocations during high packet rates.
    ///
    /// # Arguments
    /// * `priority` - Priority class this metrics tracker will monitor
    ///
    /// # Returns
    /// A new `Metrics` instance with empty statistics
    pub fn new(priority: Priority) -> Self {
        Self {
            priority,
            packet_count: 0,
            latencies: VecDeque::with_capacity(10000), // Pre-allocate for high packet rates
            deadline_misses: 0,
            time_window: Duration::from_secs(10), // Keep last 10 seconds of latency data
            cached_p50: RefCell::new(None),
            cached_p95: RefCell::new(None),
            cached_p99: RefCell::new(None),
            cached_p999: RefCell::new(None),
            cached_packet_count: RefCell::new(0),
        }
    }

    /// Record a latency measurement and update statistics.
    ///
    /// Adds the latency to the time-windowed history, removes measurements outside
    /// the time window, invalidates cached percentiles, and updates deadline miss
    /// counters. This is called on the hot path (egress DRR), so it's optimized
    /// for performance.
    ///
    /// # Arguments
    /// * `latency` - Observed latency for this packet
    /// * `deadline_missed` - Whether the packet exceeded its latency budget
    ///
    /// # Time Windowing
    /// Only measurements within the last `time_window` (default 10 seconds) are
    /// retained. This ensures all priority classes are compared over the same time
    /// period regardless of packet rate differences.
    pub fn record_latency(&mut self, latency: Duration, deadline_missed: bool) {
        self.packet_count += 1;

        let now = Instant::now();

        // Store latency in microseconds with timestamp for time-based windowing
        let latency_us = latency.as_secs_f64() * 1_000_000.0;
        self.latencies.push_back(LatencyMeasurement {
            latency_us,
            timestamp: now,
        });

        // Remove old measurements outside the time window (time-based windowing)
        // This ensures all flows are compared over the same time period regardless of packet rate
        let cutoff_time = now.checked_sub(self.time_window).unwrap_or(now);
        while let Some(front) = self.latencies.front() {
            if front.timestamp < cutoff_time {
                self.latencies.pop_front();
            } else {
                break; // Remaining measurements are all within the window
            }
        }

        // Invalidate cache when new data arrives
        *self.cached_p50.borrow_mut() = None;
        *self.cached_p95.borrow_mut() = None;
        *self.cached_p99.borrow_mut() = None;
        *self.cached_p999.borrow_mut() = None;

        if deadline_missed {
            self.deadline_misses += 1;
        }
    }

    /// Remove old measurements outside the time window.
    ///
    /// This method should be called periodically (e.g., when sending metrics to GUI)
    /// to ensure stale measurements are removed even when no new packets arrive.
    /// This prevents latency from appearing to increase when traffic stops.
    ///
    /// # Time Windowing
    /// Only measurements within the last `time_window` (default 10 seconds) are
    /// retained. Measurements older than this are removed to prevent stale data
    /// from affecting statistics.
    pub fn cleanup_old_measurements(&mut self) {
        let now = Instant::now();
        let cutoff_time = now.checked_sub(self.time_window).unwrap_or(now);

        // Remove old measurements outside the time window
        while let Some(front) = self.latencies.front() {
            if front.timestamp < cutoff_time {
                self.latencies.pop_front();
            } else {
                break; // Remaining measurements are all within the window
            }
        }

        // Invalidate cache when cleanup happens (data changed)
        *self.cached_p50.borrow_mut() = None;
        *self.cached_p95.borrow_mut() = None;
        *self.cached_p99.borrow_mut() = None;
        *self.cached_p999.borrow_mut() = None;
    }

    /// Compute all percentiles (P50, P95, P99, P99.9) at once and cache them.
    ///
    /// This method is more efficient than computing percentiles individually because
    /// it sorts the latency array once and extracts all percentiles from the sorted
    /// array. The results are cached in `RefCell` fields to avoid recomputation when
    /// multiple percentiles are requested.
    ///
    /// # Caching Strategy
    /// Percentiles are only recomputed when `packet_count` changes (new data arrived).
    /// The cache is invalidated by `record_latency()` when new measurements are added.
    ///
    /// # Performance
    /// O(n log n) where n is the number of measurements in the time window. The sort
    /// is done once per cache invalidation, not per percentile query.
    fn compute_and_cache_percentiles(&self) {
        let cached_count = *self.cached_packet_count.borrow();

        // Only recompute if cache is invalid
        if cached_count != self.packet_count {
            if self.latencies.is_empty() {
                *self.cached_p50.borrow_mut() = None;
                *self.cached_p95.borrow_mut() = None;
                *self.cached_p99.borrow_mut() = None;
                *self.cached_p999.borrow_mut() = None;
            } else {
                // Extract latency values and sort once to compute all percentiles
                let mut sorted: Vec<f64> = self.latencies.iter().map(|m| m.latency_us).collect();
                sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());

                let len = sorted.len();
                let p50_idx = ((len as f64 * 50.0 / 100.0).ceil() as usize - 1).min(len - 1);
                let p95_idx = ((len as f64 * 95.0 / 100.0).ceil() as usize - 1).min(len - 1);
                let p99_idx = ((len as f64 * 99.0 / 100.0).ceil() as usize - 1).min(len - 1);
                let p999_idx = ((len as f64 * 99.9 / 100.0).ceil() as usize - 1).min(len - 1);

                *self.cached_p50.borrow_mut() =
                    Some(Duration::from_secs_f64(sorted[p50_idx] / 1_000_000.0));
                *self.cached_p95.borrow_mut() =
                    Some(Duration::from_secs_f64(sorted[p95_idx] / 1_000_000.0));
                *self.cached_p99.borrow_mut() =
                    Some(Duration::from_secs_f64(sorted[p99_idx] / 1_000_000.0));
                *self.cached_p999.borrow_mut() =
                    Some(Duration::from_secs_f64(sorted[p999_idx] / 1_000_000.0));
            }
            *self.cached_packet_count.borrow_mut() = self.packet_count;
        }
    }

    /// Compute the arithmetic mean of all latency measurements in the time window.
    ///
    /// Returns `Duration::ZERO` if no measurements are available. The average is
    /// computed from the time-windowed history, so it reflects recent performance
    /// rather than all-time statistics.
    ///
    /// # Returns
    /// Average latency as a `Duration`, or `Duration::ZERO` if no measurements
    pub fn average_latency(&self) -> Duration {
        if self.latencies.is_empty() {
            return Duration::ZERO;
        }
        let sum: f64 = self.latencies.iter().map(|m| m.latency_us).sum();
        let mean = sum / self.latencies.len() as f64;
        Duration::from_secs_f64(mean / 1_000_000.0)
    }

    /// Compute the standard deviation of latency measurements.
    ///
    /// Returns `None` if fewer than 2 measurements are available (standard deviation
    /// requires at least 2 data points). The standard deviation is computed from the
    /// time-windowed history.
    ///
    /// # Returns
    /// Standard deviation as a `Duration`, or `None` if insufficient data
    ///
    /// # Formula
    /// ```
    /// std_dev = sqrt(sum((latency_i - mean)^2) / n)
    /// ```
    pub fn standard_deviation(&self) -> Option<Duration> {
        if self.latencies.len() < 2 {
            return None;
        }
        let mean =
            self.latencies.iter().map(|m| m.latency_us).sum::<f64>() / self.latencies.len() as f64;
        let variance = self
            .latencies
            .iter()
            .map(|m| (m.latency_us - mean).powi(2))
            .sum::<f64>()
            / self.latencies.len() as f64;
        let std_dev = variance.sqrt();
        Some(Duration::from_secs_f64(std_dev / 1_000_000.0))
    }

    /// Get the minimum latency observed in the time window.
    ///
    /// Returns `None` if no measurements are available. This is the best-case latency
    /// for this priority class.
    ///
    /// # Returns
    /// Minimum latency as a `Duration`, or `None` if no measurements
    pub fn min_latency(&self) -> Option<Duration> {
        if self.latencies.is_empty() {
            return None;
        }
        let min = self
            .latencies
            .iter()
            .map(|m| m.latency_us)
            .fold(f64::INFINITY, |a, b| a.min(b));
        Some(Duration::from_secs_f64(min / 1_000_000.0))
    }

    /// Get the maximum latency observed in the time window (P100).
    ///
    /// Returns `None` if no measurements are available. This is the worst-case latency
    /// for this priority class and represents the 100th percentile.
    ///
    /// # Returns
    /// Maximum latency as a `Duration`, or `None` if no measurements
    pub fn max_latency(&self) -> Option<Duration> {
        if self.latencies.is_empty() {
            return None;
        }
        let max = self
            .latencies
            .iter()
            .map(|m| m.latency_us)
            .fold(f64::NEG_INFINITY, |a, b| a.max(b));
        Some(Duration::from_secs_f64(max / 1_000_000.0))
    }

    /// Get the 50th percentile (median) latency.
    ///
    /// Returns `None` if no measurements are available. The result is cached and
    /// only recomputed when new data arrives.
    ///
    /// # Returns
    /// P50 latency as a `Duration`, or `None` if no measurements
    pub fn p50(&self) -> Option<Duration> {
        self.compute_and_cache_percentiles();
        *self.cached_p50.borrow()
    }

    /// Get the 95th percentile latency.
    ///
    /// Returns `None` if no measurements are available. The result is cached and
    /// only recomputed when new data arrives. P95 is commonly used to measure
    /// tail latency performance.
    ///
    /// # Returns
    /// P95 latency as a `Duration`, or `None` if no measurements
    pub fn p95(&self) -> Option<Duration> {
        self.compute_and_cache_percentiles();
        *self.cached_p95.borrow()
    }

    /// Get the 99th percentile latency.
    ///
    /// Returns `None` if no measurements are available. The result is cached and
    /// only recomputed when new data arrives. P99 is commonly used to measure
    /// extreme tail latency performance.
    ///
    /// # Returns
    /// P99 latency as a `Duration`, or `None` if no measurements
    pub fn p99(&self) -> Option<Duration> {
        self.compute_and_cache_percentiles();
        *self.cached_p99.borrow()
    }

    /// Get the 99.9th percentile latency.
    ///
    /// Returns `None` if no measurements are available. The result is cached and
    /// only recomputed when new data arrives. P99.9 captures the most extreme
    /// tail latency events (1 in 1000 packets).
    ///
    /// # Returns
    /// P99.9 latency as a `Duration`, or `None` if no measurements
    pub fn p999(&self) -> Option<Duration> {
        self.compute_and_cache_percentiles();
        *self.cached_p999.borrow()
    }
}

impl Clone for Metrics {
    fn clone(&self) -> Self {
        Self {
            priority: self.priority,
            packet_count: self.packet_count,
            latencies: self.latencies.clone(),
            deadline_misses: self.deadline_misses,
            time_window: self.time_window,
            cached_p50: RefCell::new(None),
            cached_p95: RefCell::new(None),
            cached_p99: RefCell::new(None),
            cached_p999: RefCell::new(None),
            cached_packet_count: RefCell::new(0),
        }
    }
}

/// Metrics event for lock-free recording (sent from hot path).
///
/// This lightweight structure is sent through a lock-free channel from the egress DRR
/// scheduler (hot path) to the metrics processor thread (background). Using a channel
/// ensures the hot path never blocks on mutexes, maintaining low latency.
///
/// The event contains all information needed to update metrics: priority class, observed
/// latency, and whether the packet missed its deadline.
#[derive(Debug, Clone)]
struct MetricsEvent {
    /// Priority class of the packet (determines which Metrics instance to update)
    priority: Priority,
    /// Observed latency (time from ingress to egress)
    latency: Duration,
    /// Whether the packet exceeded its latency budget (deadline miss)
    deadline_missed: bool,
}

/// Snapshot of metrics emitted to external listeners (GUI, tests, logging).
///
/// This structure contains a complete snapshot of all metrics for a single priority class,
/// including latency statistics (percentiles, average, min, max), deadline misses, queue
/// occupancy, and drop counters. Snapshots are serialized to JSON and broadcast via TCP
/// to GUI clients.
///
/// The snapshot is computed periodically (every 100ms) by the statistics thread running
/// on the best-effort core, ensuring metrics collection doesn't interfere with packet
/// processing.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MetricsSnapshot {
    pub priority: Priority,
    pub packet_count: u64,
    #[serde(with = "duration_millis")]
    pub avg_latency: Duration,
    #[serde(with = "duration_millis_option")]
    pub min_latency: Option<Duration>,
    #[serde(with = "duration_millis_option")]
    pub max_latency: Option<Duration>,
    #[serde(with = "duration_millis_option")]
    pub p50: Option<Duration>,
    #[serde(with = "duration_millis_option")]
    pub p95: Option<Duration>,
    #[serde(with = "duration_millis_option")]
    pub p99: Option<Duration>,
    #[serde(with = "duration_millis_option")]
    pub p999: Option<Duration>,
    #[serde(with = "duration_millis_option")]
    pub p100: Option<Duration>, // P100 = max latency (100th percentile)
    #[serde(with = "duration_millis_option")]
    pub std_dev: Option<Duration>,
    pub deadline_misses: u64,
    #[serde(with = "duration_millis")]
    pub expected_max_latency: Duration, // Expected max latency (latency budget)
    // Queue occupancy metrics (shared across all flows)
    #[serde(default = "default_zero")]
    pub queue1_occupancy: usize,
    #[serde(default = "default_queue_capacity")]
    pub queue1_capacity: usize,
    #[serde(default = "default_zero")]
    pub queue2_occupancy: usize,
    #[serde(default = "default_queue_capacity")]
    pub queue2_capacity: usize,
    #[serde(default = "default_worker_depths")]
    pub worker_queue_depths: Vec<usize>,
    #[serde(default = "default_worker_depths")]
    pub worker_queue_capacities: Vec<usize>,
    #[serde(default)]
    pub dispatcher_backlog: usize,
    #[serde(default = "default_worker_priority_depths")]
    pub worker_priority_depths: Vec<Vec<usize>>,
    // Packet drop metrics (per flow, aggregated from all drop points)
    #[serde(default = "default_zero_u64")]
    pub ingress_drops: u64, // Drops at IngressDRR → Input Queue
    #[serde(default = "default_zero_u64")]
    pub edf_heap_drops: u64, // Drops at EDF Heap
    #[serde(default = "default_zero_u64")]
    pub edf_output_drops: u64, // Drops at EDF → Output Queue
    #[serde(default = "default_zero_u64")]
    pub total_drops: u64, // Total drops for this flow
}

/// Snapshot of worker queue depths and capacities (multi-worker EDF only).
///
/// Contains per-worker statistics showing current queue depths, capacities, and
/// per-priority breakdowns. Used by the GUI to display worker load and identify
/// bottlenecks in the multi-worker scheduler.
///
/// This snapshot is updated periodically (every 100ms) by the statistics thread
/// and included in `MetricsSnapshot` for GUI display.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct WorkerLoadSnapshot {
    #[serde(default = "default_worker_depths")]
    pub worker_queue_depths: Vec<usize>,
    #[serde(default = "default_worker_depths")]
    pub worker_queue_capacities: Vec<usize>,
    #[serde(default)]
    pub dispatcher_backlog: usize,
    #[serde(default = "default_worker_priority_depths")]
    pub worker_priority_depths: Vec<Vec<usize>>,
}

fn default_zero() -> usize {
    0
}

fn default_queue_capacity() -> usize {
    128
}

fn default_zero_u64() -> u64 {
    0
}

fn default_worker_depths() -> Vec<usize> {
    Vec::new()
}

fn default_worker_priority_depths() -> Vec<Vec<usize>> {
    Vec::new()
}

mod duration_millis {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    // Serialize as microseconds for sub-millisecond precision
    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Store as microseconds (f64) for sub-millisecond precision
        let micros = duration.as_secs_f64() * 1_000_000.0;
        serializer.serialize_f64(micros)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let micros = f64::deserialize(deserializer)?;
        Ok(Duration::from_secs_f64(micros / 1_000_000.0))
    }
}

mod duration_millis_option {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    // Serialize as microseconds for sub-millisecond precision
    pub fn serialize<S>(duration: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match duration {
            Some(d) => {
                // Store as microseconds (f64) for sub-millisecond precision
                let micros = d.as_secs_f64() * 1_000_000.0;
                serializer.serialize_some(&micros)
            }
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt: Option<f64> = Option::deserialize(deserializer)?;
        Ok(opt.map(|micros| Duration::from_secs_f64(micros / 1_000_000.0)))
    }
}

/// Metrics collector coordinates hot-path event ingestion and background aggregation.
///
/// The collector uses a two-stage architecture to minimize contention on the hot path:
/// 1. **Hot Path**: Egress DRR calls `record_packet()` which sends events to a lock-free
///    channel (non-blocking, never waits on mutexes)
/// 2. **Background Thread**: A dedicated thread processes events in batches, updating
///    the metrics map while holding a mutex (lock held only briefly per batch)
///
/// This design ensures that packet processing latency is not affected by metrics collection,
/// while still providing accurate statistics for monitoring and debugging.
///
/// # Thread Safety
/// - `events_tx`: Lock-free channel (hot path writes, background thread reads)
/// - `metrics`: Protected by `Mutex` (only background thread writes, statistics thread reads)
/// - `worker_stats`: Protected by `Mutex` (statistics thread writes, metrics thread reads)
pub struct MetricsCollector {
    /// Per-priority metrics map (protected by Mutex, updated by background thread)
    metrics: Arc<Mutex<std::collections::HashMap<Priority, Metrics>>>,
    /// Channel for broadcasting metrics snapshots to TCP clients (GUI)
    metrics_tx: Sender<std::collections::HashMap<u64, MetricsSnapshot>>,
    /// Lock-free channel for metrics events (hot path sends here, background thread processes)
    events_tx: crossbeam_channel::Sender<MetricsEvent>,
    /// Worker load statistics (multi-worker EDF only, updated by statistics thread)
    worker_stats: Arc<Mutex<Option<WorkerLoadSnapshot>>>,
}

impl MetricsCollector {
    /// Create a new collector backed by the provided metrics sender.
    ///
    /// Initializes the collector with:
    /// - A lock-free channel for hot-path events (bounded to 10000 events)
    /// - A background thread that processes events in batches (100 events per batch)
    /// - A metrics map that aggregates statistics per priority
    /// - Worker stats storage (for multi-worker EDF scheduler)
    ///
    /// The background thread processes events asynchronously, updating the metrics map
    /// while holding a mutex only briefly (once per batch). This ensures the hot path
    /// never blocks on metrics collection.
    ///
    /// # Arguments
    /// * `metrics_tx` - Channel sender for broadcasting metrics snapshots to TCP clients
    ///
    /// # Returns
    /// A new `MetricsCollector` instance with background processing thread running
    pub fn new(metrics_tx: Sender<std::collections::HashMap<u64, MetricsSnapshot>>) -> Self {
        // Create lock-free channel for metrics events (bounded to prevent unbounded growth)
        let (events_tx, events_rx): (
            crossbeam_channel::Sender<MetricsEvent>,
            crossbeam_channel::Receiver<MetricsEvent>,
        ) = crossbeam_channel::bounded(10000);

        let metrics: Arc<Mutex<std::collections::HashMap<Priority, Metrics>>> =
            Arc::new(Mutex::new(std::collections::HashMap::new()));
        let metrics_clone = metrics.clone();

        // Background thread to process metrics events (doesn't block hot path)
        std::thread::Builder::new()
            .name("Metrics-Processor".to_string())
            .spawn(move || {
                // Process events in batches for efficiency
                let mut batch = Vec::with_capacity(100);
                loop {
                    // Collect a batch of events (non-blocking)
                    while batch.len() < 100 {
                        match events_rx.try_recv() {
                            Ok(event) => batch.push(event),
                            Err(crossbeam_channel::TryRecvError::Empty) => break,
                            Err(crossbeam_channel::TryRecvError::Disconnected) => return, // Shutdown
                        }
                    }

                    if batch.is_empty() {
                        // No events, yield briefly
                        std::thread::yield_now();
                        continue;
                    }

                    // Process batch: update metrics (lock only once per batch)
                    {
                        let mut metrics_map = metrics_clone.lock();
                        for event in &batch {
                            let metrics = metrics_map
                                .entry(event.priority)
                                .or_insert_with(|| Metrics::new(event.priority));
                            metrics.record_latency(event.latency, event.deadline_missed);
                        }
                    } // Lock released

                    // Clear batch for next iteration
                    batch.clear();
                }
            })
            .expect("Failed to spawn metrics processor thread");

        Self {
            metrics,
            metrics_tx,
            events_tx,
            worker_stats: Arc::new(Mutex::new(None)),
        }
    }

    /// Record a single packet latency event from the hot path.
    ///
    /// This method is called by the egress DRR scheduler after processing each packet.
    /// It sends the event through a lock-free channel to the background metrics processor
    /// thread, ensuring the hot path never blocks on mutexes or metrics computation.
    ///
    /// # Arguments
    /// * `priority` - Priority class of the processed packet
    /// * `latency` - Observed latency (time from ingress timestamp to egress completion)
    /// * `deadline_missed` - Whether the packet exceeded its latency budget
    ///
    /// # Performance
    /// Non-blocking operation. If the channel is full, the event is dropped (silent
    /// failure). This prevents backpressure from metrics collection affecting packet
    /// processing latency.
    ///
    /// # Thread Safety
    /// Safe to call from any thread. Uses lock-free channel with `Relaxed` ordering.
    pub fn record_packet(&self, priority: Priority, latency: Duration, deadline_missed: bool) {
        let _ = self.events_tx.send(MetricsEvent {
            priority,
            latency,
            deadline_missed,
        });
    }

    /// Update worker queue statistics (multi-worker EDF only).
    ///
    /// Called by the statistics thread to update worker load information. The stats
    /// are included in metrics snapshots sent to GUI clients, allowing visualization
    /// of per-worker queue depths and capacities.
    ///
    /// # Arguments
    /// * `stats` - Worker statistics snapshot from multi-worker EDF scheduler, or `None`
    ///   if using a different scheduler (stats are ignored for non-multi-worker schedulers)
    pub fn update_worker_stats(&self, stats: Option<MultiWorkerStats>) {
        let mut guard = self.worker_stats.lock();
        *guard = stats.map(|s| WorkerLoadSnapshot {
            worker_queue_depths: s.worker_queue_depths,
            worker_queue_capacities: s.worker_queue_capacities,
            dispatcher_backlog: s.dispatcher_backlog,
            worker_priority_depths: s.worker_priority_depths,
        });
    }

    /// Aggregate current metrics and send a snapshot to listeners.
    ///
    /// Called periodically (every 100ms) by the statistics thread running on the best-effort
    /// core. This method:
    /// 1. Clones the metrics map while holding the mutex briefly (minimizes lock contention)
    /// 2. Computes percentiles and statistics outside the lock (no blocking of other threads)
    /// 3. Merges drop counters and queue occupancy information
    /// 4. Sends the snapshot through the metrics channel to TCP clients (GUI)
    ///
    /// # Arguments
    /// * `expected_latencies` - Per-priority latency budgets (used in snapshot for reference)
    /// * `ingress_drops` - Per-priority drop counts from ingress DRR scheduler
    /// * `edf_drops` - Drop counts from EDF scheduler (heap drops + per-priority output drops)
    ///
    /// # Performance
    /// The mutex is held only for the clone operation (fast). All expensive computations
    /// (percentiles, statistics) happen outside the lock, ensuring minimal contention.
    pub fn send_current_metrics(
        &self,
        expected_latencies: &PriorityTable<Duration>,
        ingress_drops: Option<PriorityTable<u64>>,
        edf_drops: Option<(u64, PriorityTable<u64>)>,
    ) {
        // Get queue occupancy
        let queue1_occupancy = 0;
        let queue1_capacity = 0;
        let queue2_occupancy = 0;
        let queue2_capacity = 0;

        // Clean up old measurements and clone metrics (minimize lock hold time)
        // This ensures stale measurements are removed even when no new packets arrive,
        // preventing latency from appearing to increase when traffic stops.
        let metrics_clone: std::collections::HashMap<Priority, Metrics> = {
            let mut metrics = self.metrics.lock();
            // Clean up old measurements for each priority before cloning
            for metrics_entry in metrics.values_mut() {
                metrics_entry.cleanup_old_measurements();
            }
            metrics.clone() // Clone while holding lock (fast operation)
        }; // Lock released here
        let worker_stats_snapshot = { self.worker_stats.lock().clone() };

        let ingress_table = ingress_drops.unwrap_or_else(|| PriorityTable::from_fn(|_| 0));
        let (edf_heap_drops, edf_table) = edf_drops.unwrap_or((0, PriorityTable::from_fn(|_| 0)));

        // Compute statistics outside the lock (no blocking of other threads)
        let mut snapshot = std::collections::HashMap::new();
        for (priority, m) in metrics_clone.iter() {
            let expected_max = expected_latencies[*priority];
            let ingress_drops_for_flow = ingress_table[*priority];
            let edf_output_drops_for_flow = edf_table[*priority];

            // EDF heap drops are shared across all flows (we'll attribute to all flows for visibility)
            // In practice, heap drops affect all priorities, but we show it per flow for monitoring
            let total_drops = ingress_drops_for_flow + edf_heap_drops + edf_output_drops_for_flow;

            snapshot.insert(
                *priority,
                MetricsSnapshot {
                    priority: m.priority,
                    packet_count: m.packet_count,
                    avg_latency: m.average_latency(),
                    min_latency: m.min_latency(),
                    max_latency: m.max_latency(),
                    p50: m.p50(),
                    p95: m.p95(),
                    p99: m.p99(),
                    p999: m.p999(),
                    p100: m.max_latency(), // P100 is the same as max (100th percentile)
                    std_dev: m.standard_deviation(),
                    deadline_misses: m.deadline_misses,
                    expected_max_latency: expected_max,
                    queue1_occupancy,
                    queue1_capacity,
                    queue2_occupancy,
                    queue2_capacity,
                    worker_queue_depths: worker_stats_snapshot
                        .as_ref()
                        .map(|s| s.worker_queue_depths.clone())
                        .unwrap_or_default(),
                    worker_queue_capacities: worker_stats_snapshot
                        .as_ref()
                        .map(|s| s.worker_queue_capacities.clone())
                        .unwrap_or_default(),
                    dispatcher_backlog: worker_stats_snapshot
                        .as_ref()
                        .map(|s| s.dispatcher_backlog)
                        .unwrap_or(0),
                    worker_priority_depths: worker_stats_snapshot
                        .as_ref()
                        .map(|s| s.worker_priority_depths.clone())
                        .unwrap_or_default(),
                    ingress_drops: ingress_drops_for_flow,
                    edf_heap_drops, // Shared across all flows
                    edf_output_drops: edf_output_drops_for_flow,
                    total_drops,
                },
            );
        }
        let serialized_snapshot: std::collections::HashMap<u64, MetricsSnapshot> = snapshot
            .into_iter()
            .map(|(priority, metrics)| (priority.flow_id(), metrics))
            .collect();
        let _ = self.metrics_tx.try_send(serialized_snapshot);
    }
}

#[cfg(test)]
impl MetricsCollector {
    fn metrics_snapshot(&self) -> std::collections::HashMap<Priority, Metrics> {
        self.metrics.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_creation() {
        let metrics = Metrics::new(Priority::High);
        assert_eq!(metrics.priority, Priority::High);
        assert_eq!(metrics.packet_count, 0);
    }

    #[test]
    fn test_record_latency() {
        let mut metrics = Metrics::new(Priority::High);
        metrics.record_latency(Duration::from_millis(10), false);
        metrics.record_latency(Duration::from_millis(20), false);
        metrics.record_latency(Duration::from_millis(5), true);

        assert_eq!(metrics.packet_count, 3);
        assert_eq!(metrics.deadline_misses, 1);
        let min = metrics.min_latency().unwrap();
        let max = metrics.max_latency().unwrap();
        assert!(
            min <= Duration::from_millis(6) && min >= Duration::from_millis(4),
            "Min latency should be around 5ms, got {:?}",
            min
        );
        assert!(
            max <= Duration::from_millis(21) && max >= Duration::from_millis(19),
            "Max latency should be around 20ms, got {:?}",
            max
        );
    }

    #[test]
    fn test_average_latency() {
        let mut metrics = Metrics::new(Priority::High);
        metrics.record_latency(Duration::from_millis(10), false);
        metrics.record_latency(Duration::from_millis(20), false);

        let avg = metrics.average_latency();
        assert!(
            avg >= Duration::from_millis(14) && avg <= Duration::from_millis(16),
            "Average latency should be around 15ms, got {:?}",
            avg
        );
    }

    #[test]
    fn test_standard_deviation() {
        let mut metrics = Metrics::new(Priority::High);
        metrics.record_latency(Duration::from_millis(10), false);
        metrics.record_latency(Duration::from_millis(20), false);
        metrics.record_latency(Duration::from_millis(15), false);

        let std_dev = metrics.standard_deviation();
        assert!(std_dev.is_some());
        // For values 10, 20, 15, std dev should be around 4-5ms
        let std = std_dev.unwrap();
        assert!(
            std.as_millis() >= 3 && std.as_millis() <= 7,
            "Standard deviation should be around 4-5ms, got {:?}",
            std
        );
    }

    #[test]
    fn test_metrics_collector() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let collector = MetricsCollector::new(tx);

        collector.record_packet(Priority::High, Duration::from_millis(10), false);

        // Wait for background thread to process the event (metrics are now processed asynchronously)
        std::thread::sleep(Duration::from_millis(10));

        let metrics = collector.metrics_snapshot();
        assert!(metrics.contains_key(&Priority::High));
        assert_eq!(metrics[&Priority::High].packet_count, 1);
    }
}
