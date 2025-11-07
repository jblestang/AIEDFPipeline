use crossbeam_channel::{self, Sender};
use parking_lot::Mutex;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Latency measurement with timestamp for time-based windowing
#[derive(Debug, Clone)]
struct LatencyMeasurement {
    latency_us: f64,
    timestamp: Instant,
}

#[derive(Debug)]
pub struct Metrics {
    pub flow_id: u64,
    pub packet_count: u64,
    pub latencies: VecDeque<LatencyMeasurement>, // Store latencies with timestamps for time-based windowing
    pub deadline_misses: u64,
    pub last_update: Instant,
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
            flow_id: 0,
            packet_count: 0,
            latencies: VecDeque::with_capacity(10000), // Pre-allocate for high packet rates
            deadline_misses: 0,
            last_update: Instant::now(),
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
    pub fn new(flow_id: u64) -> Self {
        Self {
            flow_id,
            packet_count: 0,
            latencies: VecDeque::with_capacity(10000), // Pre-allocate for high packet rates
            deadline_misses: 0,
            last_update: Instant::now(),
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

        self.last_update = now;
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
        let mean = self.latencies.iter().map(|m| m.latency_us).sum::<f64>() / self.latencies.len() as f64;
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
        let min = self.latencies.iter().map(|m| m.latency_us).fold(f64::INFINITY, |a, b| a.min(b));
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

    #[allow(dead_code)]
    pub fn get_latency_history(&self) -> Vec<f64> {
        self.latencies.iter().map(|m| m.latency_us).collect()
    }

    pub fn get_recent_latencies(&self, count: usize) -> Vec<f64> {
        let start = self.latencies.len().saturating_sub(count);
        self.latencies.range(start..).map(|m| m.latency_us).collect()
    }

    #[allow(dead_code)]
    pub fn percentile(&self, percentile: f64) -> Option<Duration> {
        // For common percentiles, use cache; otherwise compute on demand
        match percentile {
            50.0 => self.p50(),
            95.0 => self.p95(),
            99.0 => self.p99(),
            99.9 => self.p999(),
            _ => {
                if self.latencies.is_empty() {
                    return None;
                }
                let mut sorted: Vec<f64> = self.latencies.iter().map(|m| m.latency_us).collect();
                sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let index = (sorted.len() as f64 * percentile / 100.0).ceil() as usize - 1;
                let index = index.min(sorted.len() - 1);
                Some(Duration::from_secs_f64(sorted[index] / 1_000_000.0))
            }
        }
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
            flow_id: self.flow_id,
            packet_count: self.packet_count,
            latencies: self.latencies.clone(),
            deadline_misses: self.deadline_misses,
            last_update: self.last_update,
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
    flow_id: u64,
    latency: Duration,
    deadline_missed: bool,
    expected_max_latency: Duration,
}

pub struct MetricsCollector {
    metrics: Arc<Mutex<std::collections::HashMap<u64, Metrics>>>,
    metrics_tx: Sender<std::collections::HashMap<u64, MetricsSnapshot>>,
    // Lock-free channel for metrics events (hot path sends here, background thread processes)
    events_tx: crossbeam_channel::Sender<MetricsEvent>,
    queue1: Option<Arc<crate::queue::Queue>>,
    queue2: Option<Arc<crate::queue::Queue>>,
}

// Helper struct to send metrics data
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MetricsSnapshot {
    pub flow_id: u64,
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
    #[serde(skip, default = "default_instant")]
    #[allow(dead_code)]
    pub last_update: Instant,
    // Store recent latency history for plotting (last 1000 samples)
    #[serde(skip)]
    #[allow(dead_code)]
    pub recent_latencies: Vec<f64>,
}

fn default_instant() -> Instant {
    Instant::now()
}

fn default_zero() -> usize {
    0
}

fn default_queue_capacity() -> usize {
    128
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

impl MetricsCollector {
    pub fn new(
        metrics_tx: Sender<std::collections::HashMap<u64, MetricsSnapshot>>,
        queue1: Option<Arc<crate::queue::Queue>>,
        queue2: Option<Arc<crate::queue::Queue>>,
    ) -> Self {
        // Create lock-free channel for metrics events (bounded to prevent unbounded growth)
        let (events_tx, events_rx) = crossbeam_channel::bounded(10000);
        
        let metrics: Arc<Mutex<std::collections::HashMap<u64, Metrics>>> = Arc::new(Mutex::new(std::collections::HashMap::new()));
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
                                .entry(event.flow_id)
                                .or_insert_with(|| Metrics::new(event.flow_id));
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
            queue1,
            queue2,
        }
    }

    /// Record packet metrics (LOCK-FREE: sends event to channel, never blocks)
    /// This is called from the hot path (EgressDRR) and must not block
    pub fn record_packet(
        &self,
        flow_id: u64,
        latency: Duration,
        deadline_missed: bool,
        expected_max_latency: Duration,
    ) {
        // Send event to lock-free channel (non-blocking, never waits on mutex)
        // If channel is full, drop the event (better than blocking packet processing)
        let _ = self.events_tx.try_send(MetricsEvent {
            flow_id,
            latency,
            deadline_missed,
            expected_max_latency,
        });
        
        // Note: We don't send snapshots here anymore - that's done by the statistics thread
        // This eliminates all mutex contention from the hot path
    }

    /// Force send current metrics (useful for periodic updates)
    /// OPTIMIZATION: Minimize lock hold time to reduce priority inversion risk
    /// Clone metrics data quickly, then compute statistics outside the lock
    pub fn send_current_metrics(
        &self,
        flow_id_to_expected_latency: &std::collections::HashMap<u64, Duration>,
    ) {
        // Get queue occupancy (no lock needed)
        let queue1_occupancy = self.queue1.as_ref().map(|q| q.occupancy()).unwrap_or(0);
        let queue1_capacity = self.queue1.as_ref().map(|q| q.capacity()).unwrap_or(0);
        let queue2_occupancy = self.queue2.as_ref().map(|q| q.occupancy()).unwrap_or(0);
        let queue2_capacity = self.queue2.as_ref().map(|q| q.capacity()).unwrap_or(0);

        // Quick clone to minimize lock hold time (reduces priority inversion risk)
        // Statistics computation happens outside the lock
        let metrics_clone: std::collections::HashMap<u64, Metrics> = {
            let metrics = self.metrics.lock();
            metrics.clone() // Clone while holding lock (fast operation)
        }; // Lock released here

        // Compute statistics outside the lock (no blocking of other threads)
        let mut snapshot = std::collections::HashMap::new();
        for (fid, m) in metrics_clone.iter() {
            let expected_max = flow_id_to_expected_latency
                .get(fid)
                .copied()
                .unwrap_or(Duration::from_millis(200));
            snapshot.insert(
                *fid,
                MetricsSnapshot {
                    flow_id: m.flow_id,
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
                    last_update: m.last_update,
                    recent_latencies: m.get_recent_latencies(1000),
                },
            );
        }
        let _ = self.metrics_tx.try_send(snapshot);
    }

    #[allow(dead_code)]
    pub fn get_metrics_snapshot(
        &self,
        flow_id_to_expected_latency: &std::collections::HashMap<u64, Duration>,
    ) -> std::collections::HashMap<u64, MetricsSnapshot> {
        // Get queue occupancy
        let queue1_occupancy = self.queue1.as_ref().map(|q| q.occupancy()).unwrap_or(0);
        let queue1_capacity = self.queue1.as_ref().map(|q| q.capacity()).unwrap_or(0);
        let queue2_occupancy = self.queue2.as_ref().map(|q| q.occupancy()).unwrap_or(0);
        let queue2_capacity = self.queue2.as_ref().map(|q| q.capacity()).unwrap_or(0);

        let metrics = self.metrics.lock();
        let mut snapshot = std::collections::HashMap::new();
        for (flow_id, m) in metrics.iter() {
            let expected_max = flow_id_to_expected_latency
                .get(flow_id)
                .copied()
                .unwrap_or(Duration::from_millis(100));
            snapshot.insert(
                *flow_id,
                MetricsSnapshot {
                    flow_id: m.flow_id,
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
                    last_update: m.last_update,
                    recent_latencies: m.get_recent_latencies(1000),
                },
            );
        }
        snapshot
    }

    #[allow(dead_code)]
    pub fn get_metrics(&self) -> std::collections::HashMap<u64, Metrics> {
        self.metrics.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_creation() {
        let metrics = Metrics::new(1);
        assert_eq!(metrics.flow_id, 1);
        assert_eq!(metrics.packet_count, 0);
    }

    #[test]
    fn test_record_latency() {
        let mut metrics = Metrics::new(1);
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
        let mut metrics = Metrics::new(1);
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
        let mut metrics = Metrics::new(1);
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
        let collector = MetricsCollector::new(tx, None, None);

        collector.record_packet(
            1,
            Duration::from_millis(10),
            false,
            Duration::from_millis(100),
        );
        let metrics = collector.get_metrics();
        assert!(metrics.contains_key(&1));
        assert_eq!(metrics[&1].packet_count, 1);
    }
}
