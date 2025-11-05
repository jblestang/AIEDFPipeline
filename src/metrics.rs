use crossbeam_channel::{self, Sender};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct Metrics {
    pub flow_id: u64,
    pub packet_count: u64,
    pub latencies: Vec<f64>, // Store latencies in microseconds for better precision
    pub deadline_misses: u64,
    pub last_update: Instant,
    pub max_history: usize, // Maximum number of latency samples to keep
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            flow_id: 0,
            packet_count: 0,
            latencies: Vec::new(),
            deadline_misses: 0,
            last_update: Instant::now(),
            max_history: 10000, // Keep last 10k samples
        }
    }
}

impl Metrics {
    pub fn new(flow_id: u64) -> Self {
        Self {
            flow_id,
            packet_count: 0,
            latencies: Vec::new(),
            deadline_misses: 0,
            last_update: Instant::now(),
            max_history: 10000,
        }
    }

    pub fn record_latency(&mut self, latency: Duration, deadline_missed: bool) {
        self.packet_count += 1;

        // Store latency in microseconds for better precision
        let latency_us = latency.as_secs_f64() * 1_000_000.0;
        self.latencies.push(latency_us);

        // Limit history size
        if self.latencies.len() > self.max_history {
            self.latencies.remove(0);
        }

        if deadline_missed {
            self.deadline_misses += 1;
        }

        self.last_update = Instant::now();
    }

    pub fn average_latency(&self) -> Duration {
        if self.latencies.is_empty() {
            return Duration::ZERO;
        }
        let sum: f64 = self.latencies.iter().sum();
        let mean = sum / self.latencies.len() as f64;
        Duration::from_secs_f64(mean / 1_000_000.0)
    }

    pub fn standard_deviation(&self) -> Option<Duration> {
        if self.latencies.len() < 2 {
            return None;
        }
        let mean = self.latencies.iter().sum::<f64>() / self.latencies.len() as f64;
        let variance = self
            .latencies
            .iter()
            .map(|x| (x - mean).powi(2))
            .sum::<f64>()
            / self.latencies.len() as f64;
        let std_dev = variance.sqrt();
        Some(Duration::from_secs_f64(std_dev / 1_000_000.0))
    }

    pub fn min_latency(&self) -> Option<Duration> {
        if self.latencies.is_empty() {
            return None;
        }
        let min = self.latencies.iter().fold(f64::INFINITY, |a, &b| a.min(b));
        Some(Duration::from_secs_f64(min / 1_000_000.0))
    }

    pub fn max_latency(&self) -> Option<Duration> {
        if self.latencies.is_empty() {
            return None;
        }
        let max = self
            .latencies
            .iter()
            .fold(f64::NEG_INFINITY, |a, &b| a.max(b));
        Some(Duration::from_secs_f64(max / 1_000_000.0))
    }

    #[allow(dead_code)]
    pub fn get_latency_history(&self) -> Vec<f64> {
        self.latencies.clone()
    }

    pub fn get_recent_latencies(&self, count: usize) -> Vec<f64> {
        let start = self.latencies.len().saturating_sub(count);
        self.latencies[start..].to_vec()
    }

    pub fn percentile(&self, percentile: f64) -> Option<Duration> {
        if self.latencies.is_empty() {
            return None;
        }
        let mut sorted = self.latencies.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let index = (sorted.len() as f64 * percentile / 100.0).ceil() as usize - 1;
        let index = index.min(sorted.len() - 1);
        Some(Duration::from_secs_f64(sorted[index] / 1_000_000.0))
    }

    pub fn p50(&self) -> Option<Duration> {
        self.percentile(50.0)
    }

    pub fn p95(&self) -> Option<Duration> {
        self.percentile(95.0)
    }

    pub fn p99(&self) -> Option<Duration> {
        self.percentile(99.0)
    }

    pub fn p999(&self) -> Option<Duration> {
        self.percentile(99.9)
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
            max_history: self.max_history,
        }
    }
}

pub struct MetricsCollector {
    metrics: Arc<Mutex<std::collections::HashMap<u64, Metrics>>>,
    metrics_tx: Sender<std::collections::HashMap<u64, MetricsSnapshot>>,
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
    pub fn new(metrics_tx: Sender<std::collections::HashMap<u64, MetricsSnapshot>>) -> Self {
        Self {
            metrics: Arc::new(Mutex::new(std::collections::HashMap::new())),
            metrics_tx,
        }
    }

    pub fn record_packet(
        &self,
        flow_id: u64,
        latency: Duration,
        deadline_missed: bool,
        expected_max_latency: Duration,
    ) {
        let mut metrics_map = self.metrics.lock();
        let metrics = metrics_map
            .entry(flow_id)
            .or_insert_with(|| Metrics::new(flow_id));
        metrics.record_latency(latency, deadline_missed);

        // Send updated metrics as snapshots
        let mut snapshot = std::collections::HashMap::new();
        for (fid, m) in metrics_map.iter() {
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
                    expected_max_latency, // Use the passed expected max latency
                    last_update: m.last_update,
                    recent_latencies: m.get_recent_latencies(1000), // Last 1000 samples for plotting
                },
            );
        }
        let _ = self.metrics_tx.try_send(snapshot);
    }

    /// Force send current metrics (useful for periodic updates)
    pub fn send_current_metrics(
        &self,
        flow_id_to_expected_latency: &std::collections::HashMap<u64, Duration>,
    ) {
        let metrics_map = self.metrics.lock();
        let mut snapshot = std::collections::HashMap::new();
        for (fid, m) in metrics_map.iter() {
            let expected_max = flow_id_to_expected_latency
                .get(fid)
                .copied()
                .unwrap_or(Duration::from_millis(100));
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
        let collector = MetricsCollector::new(tx);

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
