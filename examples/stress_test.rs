// Stress test to verify latency prioritization
// Tests that packets with tighter deadlines are processed first

use std::net::UdpSocket;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

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

    fn send_packets(&self, flow_id: u64, port: u16, count: usize, delay_ms: u64) {
        let socket = UdpSocket::bind("0.0.0.0:0").expect("Failed to bind sender socket");
        let packets_sent = self.packets_sent.clone();

        thread::spawn(move || {
            for i in 0..count {
                let message = format!("Flow{}_Packet{}", flow_id, i);
                if let Err(e) = socket.send_to(message.as_bytes(), format!("127.0.0.1:{}", port)) {
                    eprintln!("Error sending packet {} to flow {}: {}", i, flow_id, e);
                } else {
                    packets_sent.fetch_add(1, Ordering::Relaxed);
                }

                // Small delay between packets
                thread::sleep(Duration::from_millis(delay_ms));
            }
        });
    }

    fn receive_packets(
        &self,
        port: u16,
        _flow_id: u64,
        timeout: Duration,
    ) -> Vec<(Instant, String)> {
        let socket = UdpSocket::bind(format!("127.0.0.1:{}", port))
            .expect(&format!("Failed to bind receiver socket on port {}", port));
        socket
            .set_read_timeout(Some(timeout))
            .expect("Failed to set timeout");

        let start = Instant::now();
        let mut received = Vec::new();
        let packets_received = self.packets_received.clone();

        while start.elapsed() < timeout {
            let mut buf = [0u8; 1024];
            match socket.recv_from(&mut buf) {
                Ok((size, _)) => {
                    let data = String::from_utf8_lossy(&buf[..size]).to_string();
                    received.push((Instant::now(), data));
                    packets_received.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    // Timeout or error, continue
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }

        received
    }
}

fn analyze_results(
    results: &[(u64, Vec<(Instant, String)>)],
    test_start: Instant,
    test_duration: Duration,
) {
    println!("\n=== Stress Test Results ===\n");

    for (flow_id, packets) in results {
        println!("Flow {} Results:", flow_id);
        println!("  Packets received: {}", packets.len());

        if packets.is_empty() {
            println!("  ⚠️  No packets received!");
            continue;
        }

        // Calculate latencies (relative to test start)
        let mut latencies: Vec<Duration> = packets
            .iter()
            .map(|(time, _)| time.duration_since(test_start))
            .collect();

        latencies.sort();

        // Calculate inter-packet arrival times
        let mut inter_arrival_times = Vec::new();
        for i in 1..latencies.len() {
            inter_arrival_times.push(latencies[i] - latencies[i - 1]);
        }

        if !inter_arrival_times.is_empty() {
            let avg_inter_arrival: Duration =
                inter_arrival_times.iter().sum::<Duration>() / inter_arrival_times.len() as u32;
            println!(
                "  Average inter-arrival time: {:.2} ms",
                avg_inter_arrival.as_secs_f64() * 1000.0
            );

            let min_inter_arrival = inter_arrival_times.iter().min().unwrap();
            let max_inter_arrival = inter_arrival_times.iter().max().unwrap();
            println!(
                "  Min inter-arrival: {:.2} ms",
                min_inter_arrival.as_secs_f64() * 1000.0
            );
            println!(
                "  Max inter-arrival: {:.2} ms",
                max_inter_arrival.as_secs_f64() * 1000.0
            );
        }

        // First packet arrival time
        if let Some(&first_arrival) = latencies.first() {
            println!(
                "  First packet arrived: {:.2} ms after test start",
                first_arrival.as_secs_f64() * 1000.0
            );
        }

        println!();
    }

    // Analyze prioritization
    println!("=== Prioritization Analysis ===\n");

    // Check which flow received packets first
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
                2 => "50ms",
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

        // Verify EDF prioritization: Flow 1 (1ms) should arrive first
        if let Some((first_flow, _)) = first_packets.first() {
            if *first_flow == 1 {
                println!("\n✅ PASS: Flow 1 (1ms deadline) received packets first (correct EDF prioritization)");
            } else {
                println!(
                    "\n⚠️  WARNING: Flow {} received packets before Flow 1",
                    first_flow
                );
                println!("   Expected Flow 1 (1ms) to be prioritized by EDF scheduler");
            }
        }
    }

    // Print HDRHistogram-style statistics if available
    println!("\n=== Latency Percentiles (if using HDRHistogram) ===\n");
    println!("Note: For detailed percentile analysis, use the GUI which shows P50, P95, P99, P999");

    // Count total packets
    let total_received: usize = results.iter().map(|(_, packets)| packets.len()).sum();
    println!("\nTotal packets received: {}", total_received);
    println!("Test duration: {:.2} seconds", test_duration.as_secs_f64());
    println!(
        "Throughput: {:.2} packets/second",
        total_received as f64 / test_duration.as_secs_f64()
    );
}

fn main() {
    println!("=== AIEDF Pipeline Stress Test ===\n");
    println!("This test verifies latency prioritization by:");
    println!("  1. Sending packets to all three flows simultaneously");
    println!("  2. Verifying that Flow 1 (1ms deadline) is prioritized by EDF");
    println!("  3. Measuring packet arrival times and throughput");
    println!("  4. Testing with different data rates:");
    println!("     - Flow 1 (1ms): Low data rate (fewer packets)");
    println!("     - Flow 2 (50ms): Medium data rate");
    println!("     - Flow 3 (100ms): High data rate (more packets)\n");
    println!("Make sure the pipeline is running before starting this test!\n");
    println!("Press Enter to start...");
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).unwrap();

    let test = StressTest::new();
    let test_duration = Duration::from_secs(240); // Longer test duration

    println!("\nStarting stress test...");
    println!(
        "Sending packets for {} seconds...\n",
        test_duration.as_secs()
    );

    // Configuration - different data rates per flow (DOUBLED)
    // Flow 1 (1ms deadline): Low latency = Low data rate (fewer packets)
    // Flow 2 (50ms deadline): Medium latency = Medium data rate
    // Flow 3 (100ms deadline): High latency = High data rate (more packets)
    let packets_flow1 = 20 * test_duration.as_secs(); // Low data rate for low latency flow (doubled)
    let packets_flow2 = 100 * test_duration.as_secs(); // Medium data rate (doubled)
    let packets_flow3 = 200 * test_duration.as_secs(); // High data rate for high latency flow (doubled)

    // Different send rates to simulate different data rates (doubled = halved delays)
    let send_delay_flow1 = 50; // Send every 50ms (20 packets/sec, doubled from 10)
    let send_delay_flow2 = 10; // Send every 10ms (100 packets/sec, doubled from 50)
    let send_delay_flow3 = 5; // Send every 5ms (200 packets/sec, doubled from 100)

    // Start receiving threads for each flow
    let receiver_handles: Vec<_> = vec![(1, 9080), (2, 9081), (3, 9082)]
        .into_iter()
        .map(|(flow_id, port)| {
            let test_clone = StressTest {
                packets_sent: test.packets_sent.clone(),
                packets_received: test.packets_received.clone(),
            };
            thread::spawn(move || {
                test_clone.receive_packets(port, flow_id, test_duration + Duration::from_secs(2))
            })
        })
        .collect();

    // Small delay to ensure receivers are ready
    thread::sleep(Duration::from_millis(100));

    // Record test start time
    let test_start = Instant::now();

    println!("Sending packets with different data rates:");
    println!(
        "  Flow 1 (1ms deadline): {} packets at {} ms intervals (low data rate)",
        packets_flow1, send_delay_flow1
    );
    println!(
        "  Flow 2 (50ms deadline): {} packets at {} ms intervals (medium data rate)",
        packets_flow2, send_delay_flow2
    );
    println!(
        "  Flow 3 (100ms deadline): {} packets at {} ms intervals (high data rate)",
        packets_flow3, send_delay_flow3
    );
    println!();

    // Start sending threads for each flow with different rates
    test.send_packets(1, 8080, packets_flow1 as usize, send_delay_flow1);
    thread::sleep(Duration::from_millis(10)); // Small offset
    test.send_packets(2, 8081, packets_flow2 as usize, send_delay_flow2);
    thread::sleep(Duration::from_millis(10)); // Small offset
    test.send_packets(3, 8082, packets_flow3 as usize, send_delay_flow3);

    // Wait for test duration
    thread::sleep(test_duration);

    println!("Test completed. Waiting for receivers to finish...\n");

    // Collect results
    let results: Vec<(u64, Vec<(Instant, String)>)> = receiver_handles
        .into_iter()
        .enumerate()
        .map(|(i, handle)| {
            let flow_id = (i + 1) as u64;
            let packets = handle.join().unwrap();
            (flow_id, packets)
        })
        .collect();

    // Analyze results
    analyze_results(&results, test_start, test_duration);

    println!("\n=== Test Statistics ===\n");
    let total_sent = test.packets_sent.load(Ordering::Relaxed);
    let total_received = test.packets_received.load(Ordering::Relaxed);
    println!("Total packets sent: {}", total_sent);
    println!("Total packets received: {}", total_received);

    // Expected packets per flow
    let expected_flow1 = packets_flow1;
    let expected_flow2 = packets_flow2;
    let expected_flow3 = packets_flow3;
    let total_expected = expected_flow1 + expected_flow2 + expected_flow3;

    println!("\nExpected packets per flow:");
    println!("  Flow 1: {} (low data rate)", expected_flow1);
    println!("  Flow 2: {} (medium data rate)", expected_flow2);
    println!("  Flow 3: {} (high data rate)", expected_flow3);
    println!("  Total expected: {}", total_expected);

    if total_sent > 0 {
        let loss_rate = (1.0 - total_received as f64 / total_sent as f64) * 100.0;
        println!("Packet loss rate: {:.2}%", loss_rate);
    }

    // Calculate actual data rates
    let test_duration_secs = test_duration.as_secs_f64();
    println!("\nActual data rates:");
    for (flow_id, packets) in &results {
        let data_rate = packets.len() as f64 / test_duration_secs;
        println!(
            "  Flow {}: {:.2} packets/sec ({} packets received)",
            flow_id,
            data_rate,
            packets.len()
        );
    }
}
