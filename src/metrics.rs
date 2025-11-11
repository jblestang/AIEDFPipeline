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

    // Compute all percentiles at once and cache them (more efficient than computing individually)
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

    pub fn average_latency(&self) -> Duration {
        if self.latencies.is_empty() {
            return Duration::ZERO;
        }
        let sum: f64 = self.latencies.iter().map(|m| m.latency_us).sum();
        let mean = sum / self.latencies.len() as f64;
        Duration::from_secs_f64(mean / 1_000_000.0)
    }

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

    pub fn p50(&self) -> Option<Duration> {
        self.compute_and_cache_percentiles();
        *self.cached_p50.borrow()
    }

    pub fn p95(&self) -> Option<Duration> {
        self.compute_and_cache_percentiles();
        *self.cached_p95.borrow()
    }

    pub fn p99(&self) -> Option<Duration> {
        self.compute_and_cache_percentiles();
        *self.cached_p99.borrow()
    }

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

/// Metrics event for lock-free recording (sent from hot path)
#[derive(Debug, Clone)]
struct MetricsEvent {
    priority: Priority,
    latency: Duration,
    deadline_missed: bool,
}

/// Snapshot emitted to external listeners (GUI, tests, logging).
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
pub struct MetricsCollector {
    metrics: Arc<Mutex<std::collections::HashMap<Priority, Metrics>>>,
    metrics_tx: Sender<std::collections::HashMap<u64, MetricsSnapshot>>,
    // Lock-free channel for metrics events (hot path sends here, background thread processes)
    events_tx: crossbeam_channel::Sender<MetricsEvent>,
    worker_stats: Arc<Mutex<Option<WorkerLoadSnapshot>>>,
}

impl MetricsCollector {
    /// Create a new collector backed by the provided metrics sender and optional queue probes.
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
    /// The caller is typically the egress scheduler. Events are pushed into a lock-free channel and
    /// processed on the statistics thread, so calling this method never blocks on mutexes.
    pub fn record_packet(&self, priority: Priority, latency: Duration, deadline_missed: bool) {
        let _ = self.events_tx.send(MetricsEvent {
            priority,
            latency,
            deadline_missed,
        });
    }

    /// Update worker queue statistics (multi-worker EDF only).
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
    /// Called from the statistics thread running on the best-effort core. The method clones the
    /// underlying structures while holding the mutex briefly, computes percentiles outside the lock,
    /// and merges drop counters and queue occupancy information into the snapshot map.
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

        // Quick clone to minimize lock hold time (reduces priority inversion risk)
        // Statistics computation happens outside the lock
        let metrics_clone: std::collections::HashMap<Priority, Metrics> = {
            let metrics = self.metrics.lock();
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
