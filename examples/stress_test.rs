// Stress test utility for the AIEDF pipeline.
// Provides interactive latency validation as well as an automated benchmark mode
// that compares every available scheduler and emits a Markdown report.

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::cmp::Ordering as CmpOrdering;
use std::env;
use std::fs::{read_to_string, File, OpenOptions};
use std::io::{self, Write};
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_RESULTS_PATH: &str = "stress_results.csv";
const DEFAULT_REPORT_PATH: &str = "benchmark_report.md";
const PIPELINE_BINARY_NAME: &str = "aiedf-pipeline";

const BENCHMARK_SCHEDULERS: &[(&str, &str)] = &[
    ("single", "single"),
    ("multi-worker", "multi"),
    ("gedf", "gedf"),
    ("gedf-vd", "gedf-vd"),
];

#[derive(Debug, Clone)]
struct FlowStats {
    flow_id: u64,
    first_arrival: Option<Duration>,
    p95: Option<Duration>,
}

#[derive(Debug, Clone)]
struct StressRunRecord {
    label: String,
    timestamp: u64,
    total_received: usize,
    throughput_pps: f64,
    flow1_first_ms: Option<f64>,
    flow1_p95_ms: Option<f64>,
    loss_percent: Option<f64>,
}

#[derive(Clone)]
struct StressConfig {
    duration: Duration,
    label: String,
    results_path: Option<PathBuf>,
    summary_only: Option<PathBuf>,
    benchmark_all: bool,
    report_path: Option<PathBuf>,
    pipeline_path: Option<PathBuf>,
    high_only: bool,
}

impl StressConfig {
    fn parse() -> Self {
        let mut duration_secs: u64 = 120;
        let mut label = "default".to_string();
        let mut results_path: Option<PathBuf> = None;
        let mut summary_only: Option<PathBuf> = None;
        let mut benchmark_all = false;
        let mut report_path: Option<PathBuf> = None;
        let mut pipeline_path: Option<PathBuf> = None;
        let mut high_only = false;

        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            if let Some((key, value)) = arg.split_once('=') {
                match key {
                    "--duration" => {
                        if let Ok(secs) = value.parse::<u64>() {
                            duration_secs = secs.max(1);
                        }
                    }
                    "--label" => {
                        label = value.to_string();
                    }
                    "--results" => {
                        results_path = Some(PathBuf::from(value));
                    }
                    "--summary-only" => {
                        summary_only = Some(PathBuf::from(value));
                    }
                    "--benchmark-all" => {
                        benchmark_all = value.parse::<bool>().unwrap_or(true);
                    }
                    "--report" => {
                        report_path = Some(PathBuf::from(value));
                    }
                    "--pipeline" => {
                        pipeline_path = Some(PathBuf::from(value));
                    }
                    "--help" | "-h" => {
                        Self::print_help();
                        std::process::exit(0);
                    }
                    _ => {}
                }
            } else {
                // Handle flags without values
                match arg.as_str() {
                    "--high-only" => {
                        high_only = true;
                    }
                    "--duration" => {
                        if let Some(value) = args.next() {
                            if let Ok(secs) = value.parse::<u64>() {
                                duration_secs = secs.max(1);
                            }
                        }
                    }
                    "--label" => {
                        if let Some(value) = args.next() {
                            label = value;
                        }
                    }
                    "--results" => {
                        if let Some(value) = args.next() {
                            results_path = Some(PathBuf::from(value));
                        }
                    }
                    "--summary-only" => {
                        if let Some(value) = args.next() {
                            summary_only = Some(PathBuf::from(value));
                        }
                    }
                    "--benchmark-all" => {
                        benchmark_all = true;
                    }
                    "--report" => {
                        if let Some(value) = args.next() {
                            report_path = Some(PathBuf::from(value));
                        }
                    }
                    "--pipeline" => {
                        if let Some(value) = args.next() {
                            pipeline_path = Some(PathBuf::from(value));
                        }
                    }
                    "--help" | "-h" => {
                        Self::print_help();
                        std::process::exit(0);
                    }
                    other => {
                        eprintln!("Unknown argument: {}", other);
                        Self::print_help();
                        std::process::exit(1);
                    }
                }
            }
        }

        Self {
            duration: Duration::from_secs(duration_secs),
            label,
            results_path,
            summary_only,
            benchmark_all,
            report_path,
            pipeline_path,
            high_only,
        }
    }

    fn print_help() {
        println!("Usage: cargo run --example stress_test [options]");
        println!();
        println!("Options:");
        println!("  --duration <secs>       Duration of the stress test (default 120)");
        println!("  --label <name>          Label for this run (default \"default\")");
        println!("  --results <file>        Append results to CSV file");
        println!("  --summary-only <file>   Display CSV comparison summary and exit");
        println!("  --benchmark-all         Run against every scheduler and emit a report");
        println!("  --report <file>         Markdown report path (with --benchmark-all)");
        println!("  --pipeline <path>       Override pipeline binary path");
        println!("  --high-only             Only test HIGH priority flows (Flow 1)");
        println!("  -h, --help              Show this help message");
    }

    fn effective_report_path(&self) -> PathBuf {
        self.report_path
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_REPORT_PATH))
    }
}

struct StressTest {
    packets_sent: Arc<AtomicU64>,
    packets_received: Arc<AtomicU64>,
}

impl StressTest {
    fn new() -> Self {
        Self {
            packets_sent: Arc::new(AtomicU64::new(0)),
            packets_received: Arc::new(AtomicU64::new(0)),
        }
    }

    fn send_packets(
        &self,
        flow_id: u64,
        port: u16,
        count: usize,
        interval: Duration,
        progress: Option<Arc<ProgressBar>>,
    ) {
        let socket = UdpSocket::bind("0.0.0.0:0").expect("Failed to bind sender socket");
        let packets_sent = self.packets_sent.clone();
        let progress_clone = progress.clone();

        thread::spawn(move || {
            let mut rng_state = (flow_id << 32)
                ^ (count as u64)
                ^ (SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as u64);
            for i in 0..count {
                rng_state ^= rng_state << 7;
                rng_state ^= rng_state >> 9;
                let random = rng_state.wrapping_add(i as u64);
                let size = 64 + (random as usize % 1337); // 64..1400 bytes
                let payload_byte = b'A' + ((i as usize + flow_id as usize) % 26) as u8;
                let payload = vec![payload_byte; size];

                if let Err(e) = socket.send_to(&payload, format!("127.0.0.1:{port}")) {
                    eprintln!("Error sending packet {i} to flow {flow_id}: {e}");
                } else {
                    packets_sent.fetch_add(1, Ordering::Relaxed);
                    if let Some(pb) = progress_clone.as_ref() {
                        pb.inc(1);
                    }
                }

                thread::sleep(interval);
            }

            if let Some(pb) = progress_clone {
                pb.finish_with_message(format!("Flow {flow_id} completed"));
            }
        });
    }

    fn receive_packets(
        &self,
        port: u16,
        _flow_id: u64,
        timeout: Duration,
    ) -> Vec<(Instant, String)> {
        let socket = UdpSocket::bind(format!("127.0.0.1:{port}"))
            .unwrap_or_else(|_| panic!("Failed to bind receiver socket on port {port}"));
        socket
            .set_read_timeout(Some(timeout))
            .expect("Failed to set timeout");

        let start = Instant::now();
        let mut received = Vec::new();
        let packets_received = self.packets_received.clone();

        while start.elapsed() < timeout {
            let mut buf = [0u8; 2048];
            match socket.recv_from(&mut buf) {
                Ok((size, _)) => {
                    let data = String::from_utf8_lossy(&buf[..size]).to_string();
                    received.push((Instant::now(), data));
                    packets_received.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }

        received
    }
}

fn percentile(latencies: &[Duration], percentile: f64) -> Option<Duration> {
    if latencies.is_empty() {
        return None;
    }
    if latencies.len() == 1 {
        return Some(latencies[0]);
    }
    let pct = percentile.clamp(0.0, 1.0);
    let rank = ((latencies.len() - 1) as f64 * pct).round() as usize;
    latencies.get(rank).cloned()
}

fn analyze_results(
    results: &[(u64, Vec<(Instant, String)>)],
    test_start: Instant,
    test_duration: Duration,
) -> Vec<FlowStats> {
    println!("\n=== Stress Test Results ===\n");

    let mut flow_stats = Vec::new();

    for (flow_id, packets) in results {
        println!("Flow {flow_id} Results:");
        println!("  Packets received: {}", packets.len());

        if packets.is_empty() {
            println!("  ⚠️  No packets received!\n");
            flow_stats.push(FlowStats {
                flow_id: *flow_id,
                first_arrival: None,
                p95: None,
            });
            continue;
        }

        let mut latencies: Vec<Duration> = packets
            .iter()
            .map(|(time, _)| time.duration_since(test_start))
            .collect();
        latencies.sort();

        let first_arrival = latencies.first().cloned();
        if let Some(first) = first_arrival {
            println!(
                "  First packet arrived: {:.2} ms after test start",
                first.as_secs_f64() * 1000.0
            );
        }

        let mut inter_arrival_times = Vec::new();
        for window in latencies.windows(2) {
            inter_arrival_times.push(window[1] - window[0]);
        }
        if !inter_arrival_times.is_empty() {
            let avg_inter_arrival: Duration =
                inter_arrival_times.iter().sum::<Duration>() / inter_arrival_times.len() as u32;
            println!(
                "  Average inter-arrival: {:.2} ms",
                avg_inter_arrival.as_secs_f64() * 1000.0
            );
            if let Some(min) = inter_arrival_times.iter().min() {
                println!("  Min inter-arrival: {:.2} ms", min.as_secs_f64() * 1000.0);
            }
            if let Some(max) = inter_arrival_times.iter().max() {
                println!("  Max inter-arrival: {:.2} ms", max.as_secs_f64() * 1000.0);
            }
        }

        let p95 = percentile(&latencies, 0.95);
        if let Some(p95_val) = p95 {
            println!("  P95 latency: {:.2} ms", p95_val.as_secs_f64() * 1000.0);
        }
        if let Some(p99_val) = percentile(&latencies, 0.99) {
            println!("  P99 latency: {:.2} ms", p99_val.as_secs_f64() * 1000.0);
        }

        println!();

        flow_stats.push(FlowStats {
            flow_id: *flow_id,
            first_arrival,
            p95,
        });
    }

    println!("=== Prioritization Analysis ===\n");
    let mut first_packets: Vec<_> = results
        .iter()
        .filter_map(|(flow_id, packets)| {
            packets
                .first()
                .map(|(time, _)| (*flow_id, time.duration_since(test_start)))
        })
        .collect();

    if !first_packets.is_empty() {
        first_packets.sort_by_key(|(_, duration)| *duration);
        println!("Packet arrival order (first packet per flow):");
        for (i, (flow_id, duration)) in first_packets.iter().enumerate() {
            let latency_budget = match flow_id {
                1 => "1ms",
                2 => "10ms",
                3 => "100ms",
                _ => "unknown",
            };
            println!(
                "  {}. Flow {} ({} budget) - {:.2} ms",
                i + 1,
                flow_id,
                latency_budget,
                duration.as_secs_f64() * 1000.0
            );
        }

        if let Some((first_flow, _)) = first_packets.first() {
            if *first_flow == 1 {
                println!(
                    "\n✅ PASS: Flow 1 (1ms deadline) arrived first (expected EDF ordering)\n"
                );
            } else {
                println!(
                    "\n⚠️  WARNING: Flow {} arrived before Flow 1 (expected EDF prioritization)\n",
                    first_flow
                );
            }
        }
    }

    println!("=== Aggregate Metrics ===\n");
    let total_received: usize = results.iter().map(|(_, packets)| packets.len()).sum();
    println!("Total packets received: {}", total_received);
    println!("Test duration: {:.2} seconds", test_duration.as_secs_f64());
    println!(
        "Throughput: {:.2} packets/second\n",
        total_received as f64 / test_duration.as_secs_f64()
    );

    flow_stats
}

fn append_record(path: &Path, record: &StressRunRecord) -> io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;

    if file.metadata()?.len() == 0 {
        writeln!(
            file,
            "label,timestamp,total_received,throughput_pps,flow1_first_ms,flow1_p95_ms,loss_percent"
        )?;
    }

    writeln!(
        file,
        "{},{},{},{:.3},{},{},{}",
        record.label,
        record.timestamp,
        record.total_received,
        record.throughput_pps,
        format_opt(record.flow1_first_ms),
        format_opt(record.flow1_p95_ms),
        format_opt(record.loss_percent)
    )?;
    Ok(())
}

fn format_opt(value: Option<f64>) -> String {
    value
        .map(|v| format!("{:.3}", v))
        .unwrap_or_else(|| "NA".to_string())
}

fn parse_record_line(line: &str) -> Option<StressRunRecord> {
    let parts: Vec<&str> = line.split(',').collect();
    if parts.len() < 7 {
        return None;
    }
    let parse_opt = |input: &str| -> Option<f64> {
        let trimmed = input.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("NA") {
            None
        } else {
            trimmed.parse::<f64>().ok()
        }
    };

    Some(StressRunRecord {
        label: parts[0].to_string(),
        timestamp: parts[1].parse().unwrap_or_default(),
        total_received: parts[2].parse().unwrap_or_default(),
        throughput_pps: parts[3].parse().unwrap_or_default(),
        flow1_first_ms: parse_opt(parts[4]),
        flow1_p95_ms: parse_opt(parts[5]),
        loss_percent: parse_opt(parts[6]),
    })
}

fn show_summary(path: &Path) -> io::Result<()> {
    let data = read_to_string(path)?;
    let mut records: Vec<StressRunRecord> =
        data.lines().skip(1).filter_map(parse_record_line).collect();

    if records.is_empty() {
        println!("No records found in {}", path.display());
        return Ok(());
    }

    records.sort_by(|a, b| a.label.cmp(&b.label));

    println!(
        "\n=== Scheduler Comparison Summary ({}) ===",
        path.display()
    );
    println!(
        "{:<18} {:>14} {:>16} {:>16} {:>10}",
        "Label", "Throughput", "Flow1 P95 (ms)", "Flow1 First (ms)", "Loss %"
    );
    println!("{}", "-".repeat(78));
    for rec in &records {
        println!(
            "{:<18} {:>14.2} {:>16} {:>16} {:>10}",
            rec.label,
            rec.throughput_pps,
            rec.flow1_p95_ms
                .map(|v| format!("{:.2}", v))
                .unwrap_or_else(|| "NA".to_string()),
            rec.flow1_first_ms
                .map(|v| format!("{:.2}", v))
                .unwrap_or_else(|| "NA".to_string()),
            rec.loss_percent
                .map(|v| format!("{:.2}", v))
                .unwrap_or_else(|| "NA".to_string()),
        );
    }

    if let Some(best) = records
        .iter()
        .filter(|r| r.flow1_p95_ms.is_some())
        .min_by(|a, b| {
            a.flow1_p95_ms
                .partial_cmp(&b.flow1_p95_ms)
                .unwrap_or(CmpOrdering::Equal)
        })
    {
        println!(
            "\nBest Flow1 P95 latency: {} ({:.2} ms)",
            best.label,
            best.flow1_p95_ms.unwrap()
        );
    }
    if let Some(best_thr) = records.iter().max_by(|a, b| {
        a.throughput_pps
            .partial_cmp(&b.throughput_pps)
            .unwrap_or(CmpOrdering::Equal)
    }) {
        println!(
            "Highest throughput: {} ({:.2} pkt/s)",
            best_thr.label, best_thr.throughput_pps
        );
    }

    Ok(())
}

fn current_pipeline_path() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    let examples_dir = exe.parent()?; // .../target/debug/examples
    let target_dir = examples_dir.parent()?; // .../target/debug
    #[allow(unused_mut)]
    let mut candidate = target_dir.join(PIPELINE_BINARY_NAME);
    if candidate.exists() {
        return Some(candidate);
    }
    #[cfg(windows)]
    {
        candidate.set_extension("exe");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn spawn_pipeline(path: &Path, scheduler: &str, metrics_port: u16) -> io::Result<Child> {
    let mut cmd = Command::new(path);
    cmd.arg(format!("--scheduler={scheduler}"))
        .arg(format!("--metrics-bind=127.0.0.1:{metrics_port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd.spawn()
}

fn run_stress(label: &str, config: &StressConfig, interactive: bool) -> Option<StressRunRecord> {
    if interactive {
        println!("=== AIEDF Pipeline Stress Test ===\n");
        if config.high_only {
            println!("This test measures HIGH priority flow performance:");
            println!("  1. Sending packets to Flow 1 (HIGH priority, 1ms deadline) only");
            println!("  2. Measuring packet arrival times and throughput");
            println!("  3. Recording metrics for scheduler-to-scheduler comparisons.\n");
        } else {
            println!("This test verifies latency prioritization by:");
            println!("  1. Sending packets to all three flows simultaneously");
            println!("  2. Verifying that Flow 1 (1ms deadline) is prioritized by EDF");
            println!("  3. Measuring packet arrival times and throughput");
            println!("  4. Recording metrics for scheduler-to-scheduler comparisons.\n");
        }
        println!("Make sure the pipeline is running before starting this test!");
        println!("Press Enter to start...");
        let mut input = String::new();
        let _ = std::io::stdin().read_line(&mut input);
    } else {
        println!("\n>>> Starting automated run for scheduler \"{label}\"");
    }

    let test = StressTest::new();
    let test_duration = config.duration;

    println!(
        "\nRunning stress test \"{label}\" for {} seconds...",
        test_duration.as_secs()
    );

    let factor = 10;
    let rate_flow1 = 64u64 * factor;
    let rate_flow2 = 128u64 * factor;
    let rate_flow3 = 256u64 * factor;

    let packets_flow1 = rate_flow1 * test_duration.as_secs();
    let packets_flow2 = rate_flow2 * test_duration.as_secs();
    let packets_flow3 = rate_flow3 * test_duration.as_secs();

    let interval_flow1 = Duration::from_micros((1_000_000.0 / rate_flow1 as f64) as u64);
    let interval_flow2 = Duration::from_micros((1_000_000.0 / rate_flow2 as f64) as u64);
    let interval_flow3 = Duration::from_micros((1_000_000.0 / rate_flow3 as f64) as u64);

    // Receive packets from multiple output sockets per priority
    // Flow 1 (High): ports 9080, 9081
    // Flow 2 (Medium): ports 9082, 9083
    // Flow 3 (Low): ports 9084, 9085
    let receiver_handles: Vec<_> = if config.high_only {
        vec![
            (1, 9080),
            (1, 9081), // Flow 1: High priority (2 sockets) only
        ]
    } else {
        vec![
            (1, 9080),
            (1, 9081), // Flow 1: High priority (2 sockets)
            (2, 9082),
            (2, 9083), // Flow 2: Medium priority (2 sockets)
            (3, 9084),
            (3, 9085), // Flow 3: Low priority (2 sockets)
        ]
    }
    .into_iter()
    .map(|(flow_id, port)| {
        let test_clone = StressTest {
            packets_sent: test.packets_sent.clone(),
            packets_received: test.packets_received.clone(),
        };
        thread::spawn(move || {
            (
                flow_id,
                test_clone.receive_packets(port, flow_id, test_duration + Duration::from_secs(2)),
            )
        })
    })
    .collect();

    thread::sleep(Duration::from_millis(100));

    let test_start = Instant::now();

    println!("Configured data rates:");
    println!(
        "  Flow 1 (1ms deadline): {} packets at {:.2} ms intervals ({} pps)",
        packets_flow1,
        interval_flow1.as_secs_f64() * 1000.0,
        rate_flow1
    );
    if !config.high_only {
        println!(
            "  Flow 2 (10ms deadline): {} packets at {:.2} ms intervals ({} pps)",
            packets_flow2,
            interval_flow2.as_secs_f64() * 1000.0,
            rate_flow2
        );
        println!(
            "  Flow 3 (100ms deadline): {} packets at {:.2} ms intervals ({} pps)",
            packets_flow3,
            interval_flow3.as_secs_f64() * 1000.0,
            rate_flow3
        );
    }
    println!();

    let multi = MultiProgress::new();
    let bar_style = ProgressStyle::with_template(
        "{prefix:>8} [{bar:40.cyan/blue}] {pos:>7}/{len:7} ({percent:>3.0}%) ETA {eta}",
    )
    .unwrap()
    .progress_chars("=>-");

    let flow1_bar = Arc::new(multi.add(ProgressBar::new(packets_flow1.max(1))));
    flow1_bar.set_style(bar_style.clone());
    flow1_bar.set_prefix("Flow 1");

    let flow2_bar = if config.high_only {
        Arc::new(multi.add(ProgressBar::new(0))) // Unused, but keep for compatibility
    } else {
        Arc::new(multi.add(ProgressBar::new(packets_flow2.max(1))))
    };
    if !config.high_only {
        flow2_bar.set_style(bar_style.clone());
        flow2_bar.set_prefix("Flow 2");
    }

    let flow3_bar = if config.high_only {
        Arc::new(multi.add(ProgressBar::new(0))) // Unused, but keep for compatibility
    } else {
        Arc::new(multi.add(ProgressBar::new(packets_flow3.max(1))))
    };
    if !config.high_only {
        flow3_bar.set_style(bar_style.clone());
        flow3_bar.set_prefix("Flow 3");
    }

    let elapsed_bar = multi.add(ProgressBar::new(test_duration.as_secs().max(1)));
    elapsed_bar.set_style(
        ProgressStyle::with_template("  Elapsed [{bar:40.green/black}] {pos:>3}/{len:3}s")
            .unwrap()
            .progress_chars("=>-"),
    );

    // Send packets to multiple sockets per priority for load balancing
    // Flow 1 (High): ports 8080, 8081
    // Flow 2 (Medium): ports 8082, 8083
    // Flow 3 (Low): ports 8084, 8085
    //
    // When using multiple sockets, we divide packets evenly but double the interval
    // to maintain the same total rate (each socket sends at half rate, combined = full rate)
    // Use ceiling division to ensure we don't lose packets due to integer division
    let packets_per_socket_flow1 = (packets_flow1 as usize + 1) / 2; // Round up
    let packets_per_socket_flow2 = (packets_flow2 as usize + 1) / 2; // Round up
    let packets_per_socket_flow3 = (packets_flow3 as usize + 1) / 2; // Round up

    // Double the interval for each socket to maintain total rate (2 sockets * half rate = full rate)
    let interval_per_socket_flow1 = interval_flow1 * 2;
    let interval_per_socket_flow2 = interval_flow2 * 2;
    let interval_per_socket_flow3 = interval_flow3 * 2;

    // Flow 1: High priority (2 sockets)
    test.send_packets(
        1,
        8080,
        packets_per_socket_flow1,
        interval_per_socket_flow1,
        Some(flow1_bar.clone()),
    );
    test.send_packets(
        1,
        8081,
        packets_per_socket_flow1,
        interval_per_socket_flow1,
        Some(flow1_bar.clone()),
    );

    if !config.high_only {
        thread::sleep(Duration::from_millis(10));

        // Flow 2: Medium priority (2 sockets)
        test.send_packets(
            2,
            8082,
            packets_per_socket_flow2,
            interval_per_socket_flow2,
            Some(flow2_bar.clone()),
        );
        test.send_packets(
            2,
            8083,
            packets_per_socket_flow2,
            interval_per_socket_flow2,
            Some(flow2_bar.clone()),
        );
        thread::sleep(Duration::from_millis(10));

        // Flow 3: Low priority (2 sockets)
        test.send_packets(
            3,
            8084,
            packets_per_socket_flow3,
            interval_per_socket_flow3,
            Some(flow3_bar.clone()),
        );
        test.send_packets(
            3,
            8085,
            packets_per_socket_flow3,
            interval_per_socket_flow3,
            Some(flow3_bar.clone()),
        );
    }

    let elapsed_handle = {
        let pb = elapsed_bar.clone();
        thread::spawn(move || {
            for _ in 0..test_duration.as_secs() {
                thread::sleep(Duration::from_secs(1));
                pb.inc(1);
            }
            pb.finish_with_message("Elapsed");
        })
    };

    let _ = elapsed_handle.join();

    println!("Test completed. Waiting for receivers to finish...\n");

    // Collect results from all receivers and merge by flow_id
    let mut results_by_flow: std::collections::HashMap<u64, Vec<(Instant, String)>> =
        std::collections::HashMap::new();
    for handle in receiver_handles {
        let (flow_id, packets) = handle.join().unwrap();
        results_by_flow
            .entry(flow_id)
            .or_insert_with(Vec::new)
            .extend(packets);
    }
    // Sort packets by timestamp within each flow
    for packets in results_by_flow.values_mut() {
        packets.sort_by_key(|(time, _)| *time);
    }
    let results: Vec<(u64, Vec<(Instant, String)>)> = results_by_flow.into_iter().collect();

    let stats = analyze_results(&results, test_start, test_duration);

    println!("\n=== Test Statistics ===\n");
    let total_sent = test.packets_sent.load(Ordering::Relaxed);
    let total_received = test.packets_received.load(Ordering::Relaxed);
    println!("Total packets sent: {}", total_sent);
    println!("Total packets received: {}", total_received);

    let expected_flow1 = packets_flow1;
    let expected_flow2 = if config.high_only { 0 } else { packets_flow2 };
    let expected_flow3 = if config.high_only { 0 } else { packets_flow3 };
    let total_expected = expected_flow1 + expected_flow2 + expected_flow3;

    println!("\nExpected packets per flow:");
    println!("  Flow 1: {} (HIGH priority)", expected_flow1);
    if !config.high_only {
        println!("  Flow 2: {} (MEDIUM priority)", expected_flow2);
        println!("  Flow 3: {} (LOW priority)", expected_flow3);
    }
    println!("  Total expected: {}", total_expected);

    let loss_rate = if total_sent > 0 {
        let loss = (1.0 - total_received as f64 / total_sent as f64) * 100.0;
        println!("Packet loss rate: {:.2}%\n", loss);
        Some(loss)
    } else {
        None
    };

    let test_duration_secs = test_duration.as_secs_f64();
    println!("Actual data rates:");
    for (flow_id, packets) in &results {
        let data_rate = packets.len() as f64 / test_duration_secs;
        println!(
            "  Flow {}: {:.2} packets/sec ({} packets received)",
            flow_id,
            data_rate,
            packets.len()
        );
    }

    let flow1_stats = stats.iter().find(|s| s.flow_id == 1);
    let record = StressRunRecord {
        label: label.to_string(),
        timestamp: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        total_received: total_received as usize,
        throughput_pps: total_received as f64 / test_duration_secs,
        flow1_first_ms: flow1_stats
            .and_then(|s| s.first_arrival)
            .map(|d| d.as_secs_f64() * 1000.0),
        flow1_p95_ms: flow1_stats
            .and_then(|s| s.p95)
            .map(|d| d.as_secs_f64() * 1000.0),
        loss_percent: loss_rate,
    };

    if let Some(path) = config.results_path.clone() {
        if let Err(err) = append_record(&path, &record) {
            eprintln!("Failed to append results: {err}");
        } else if !config.benchmark_all {
            if let Err(err) = show_summary(&path) {
                eprintln!("Failed to display summary: {err}");
            }
        }
    }

    Some(record)
}

fn write_benchmark_report(path: &Path, records: &[StressRunRecord]) -> io::Result<()> {
    let mut file = File::create(path)?;
    writeln!(file, "# Scheduler Benchmark Report")?;
    writeln!(
        file,
        "_Generated at UNIX timestamp {}_\n",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    )?;

    writeln!(
        file,
        "| Scheduler | Throughput (pkt/s) | Flow1 P95 (ms) | Flow1 First (ms) | Loss % |"
    )?;
    writeln!(
        file,
        "|-----------|-------------------|----------------|------------------|--------|"
    )?;
    for rec in records {
        writeln!(
            file,
            "| {} | {:.2} | {} | {} | {} |",
            rec.label,
            rec.throughput_pps,
            rec.flow1_p95_ms
                .map(|v| format!("{:.3}", v))
                .unwrap_or_else(|| "NA".to_string()),
            rec.flow1_first_ms
                .map(|v| format!("{:.3}", v))
                .unwrap_or_else(|| "NA".to_string()),
            rec.loss_percent
                .map(|v| format!("{:.2}", v))
                .unwrap_or_else(|| "NA".to_string())
        )?;
    }

    if let Some(best_latency) =
        records
            .iter()
            .filter(|r| r.flow1_p95_ms.is_some())
            .min_by(|a, b| {
                a.flow1_p95_ms
                    .partial_cmp(&b.flow1_p95_ms)
                    .unwrap_or(CmpOrdering::Equal)
            })
    {
        writeln!(
            file,
            "\n- **Best Flow1 P95 latency:** {} ({:.3} ms)",
            best_latency.label,
            best_latency.flow1_p95_ms.unwrap()
        )?;
    }

    if let Some(best_thr) = records.iter().max_by(|a, b| {
        a.throughput_pps
            .partial_cmp(&b.throughput_pps)
            .unwrap_or(CmpOrdering::Equal)
    }) {
        writeln!(
            file,
            "- **Highest throughput:** {} ({:.2} packets/s)",
            best_thr.label, best_thr.throughput_pps
        )?;
    }

    Ok(())
}

fn run_benchmark_suite(config: &StressConfig) -> io::Result<()> {
    let pipeline_path = if let Some(path) = &config.pipeline_path {
        path.clone()
    } else {
        current_pipeline_path().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Pipeline binary not found. Consider using --pipeline <path>",
            )
        })?
    };

    println!(
        "Benchmarking all schedulers using pipeline binary: {}",
        pipeline_path.display()
    );

    let mut records = Vec::new();

    for (idx, (label, scheduler_flag)) in BENCHMARK_SCHEDULERS.iter().enumerate() {
        println!("\n==============================");
        println!("Benchmarking scheduler: {label}");
        println!("==============================");

        let metrics_port = 9900 + idx as u16;
        let mut child = spawn_pipeline(&pipeline_path, scheduler_flag, metrics_port)?;

        thread::sleep(Duration::from_secs(2));

        let record = run_stress(label, config, false);
        if let Some(record) = record {
            records.push(record);
        } else {
            eprintln!("Run for scheduler {label} did not produce a record.");
        }

        if child.kill().is_err() {
            eprintln!("Failed to terminate pipeline process for scheduler {label}");
        }
        let _ = child.wait();
        thread::sleep(Duration::from_millis(500));
    }

    if records.is_empty() {
        println!("No benchmark data collected.");
        return Ok(());
    }

    let report_path = config.effective_report_path();
    write_benchmark_report(&report_path, &records)?;
    println!("\nBenchmark report written to {}", report_path.display());

    if let Some(results_path) = config.results_path.clone() {
        if let Err(err) = show_summary(&results_path) {
            eprintln!("Failed to display updated summary: {err}");
        }
    }

    Ok(())
}

fn main() {
    let mut config = StressConfig::parse();

    if let Some(summary_path) = config.summary_only.clone() {
        if let Err(err) = show_summary(&summary_path) {
            eprintln!("Failed to show summary: {err}");
            std::process::exit(1);
        }
        return;
    }

    if config.benchmark_all {
        if config.results_path.is_none() {
            config.results_path = Some(PathBuf::from(DEFAULT_RESULTS_PATH));
        }
        if let Err(err) = run_benchmark_suite(&config) {
            eprintln!("Benchmark suite failed: {err}");
            std::process::exit(1);
        }
        return;
    }

    if config.results_path.is_none() {
        config.results_path = Some(PathBuf::from(DEFAULT_RESULTS_PATH));
    }

    let _ = run_stress(&config.label, &config, true);
}
