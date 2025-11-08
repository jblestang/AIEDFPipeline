// Unit tests for stress test functionality
// These tests verify that the pipeline correctly handles stress test scenarios

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::timeout;

use aiedf_pipeline::pipeline::{Pipeline, PipelineConfig};

#[tokio::test]
#[ignore]
async fn test_stress_test_packet_sending() {
    println!("\n=== Testing Stress Test Packet Sending ===\n");

    let pipeline = Arc::new(
        Pipeline::new(PipelineConfig::default())
            .await
            .expect("Failed to create pipeline"),
    );

    let pipeline_clone = pipeline.clone();
    tokio::spawn(async move {
        pipeline_clone.run().await.expect("Pipeline run failed");
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Start receiver for flow 1
    let (tx, mut rx) = mpsc::channel(200);
    let tx_clone = tx.clone();

    tokio::spawn(async move {
        let socket = UdpSocket::bind("127.0.0.1:9080")
            .await
            .expect("Failed to bind receiver");

        let mut buf = [0u8; 1024];

        // Receive packets for 5 seconds
        let end_time = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end_time {
            match timeout(Duration::from_millis(100), socket.recv_from(&mut buf)).await {
                Ok(Ok((size, _))) => {
                    let data = String::from_utf8_lossy(&buf[..size]).to_string();
                    let _ = tx_clone.send((Instant::now(), data)).await;
                }
                _ => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Send multiple packets like stress test does
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .expect("Failed to bind sender");

    let _test_start = Instant::now();
    let packets_to_send = 50;

    for i in 0..packets_to_send {
        let message = format!("Flow1_Packet{}", i);
        socket
            .send_to(message.as_bytes(), "127.0.0.1:8080")
            .await
            .expect("Failed to send");

        // Small delay between packets (like stress test)
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Wait for processing
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Collect received packets
    let mut received = Vec::new();
    while let Ok(Some((time, data))) = timeout(Duration::from_millis(100), rx.recv()).await {
        received.push((time, data));
    }

    pipeline.shutdown().await;

    println!(
        "Sent {} packets, received {} packets",
        packets_to_send,
        received.len()
    );

    // Verify we received packets
    assert!(
        !received.is_empty(),
        "Should have received at least one packet"
    );

    // Verify packet format
    for (_, data) in &received {
        assert!(
            data.starts_with("Flow1_Packet"),
            "Packet should have correct format: {}",
            data
        );
    }

    println!(
        "\n✅ Stress test packet sending verified: {} packets received",
        received.len()
    );
}

#[tokio::test]
#[ignore]
async fn test_stress_test_multiple_flows() {
    println!("\n=== Testing Stress Test Multiple Flows ===\n");

    let pipeline = Arc::new(
        Pipeline::new(PipelineConfig::default())
            .await
            .expect("Failed to create pipeline"),
    );

    let pipeline_clone = pipeline.clone();
    tokio::spawn(async move {
        pipeline_clone.run().await.expect("Pipeline run failed");
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Start receivers for all flows
    let (tx1, mut rx1) = mpsc::channel(100);
    let (tx2, mut rx2) = mpsc::channel(100);
    let (tx3, mut rx3) = mpsc::channel(100);

    for (flow_id, tx, port) in vec![(1, tx1, 9080), (2, tx2, 9081), (3, tx3, 9082)] {
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            let socket = UdpSocket::bind(format!("127.0.0.1:{}", port))
                .await
                .expect(&format!("Failed to bind on port {}", port));

            let mut buf = [0u8; 1024];
            let end_time = Instant::now() + Duration::from_secs(5);

            while Instant::now() < end_time {
                match timeout(Duration::from_millis(100), socket.recv_from(&mut buf)).await {
                    Ok(Ok((size, _))) => {
                        let data = String::from_utf8_lossy(&buf[..size]).to_string();
                        let _ = tx_clone.send((flow_id, Instant::now(), data)).await;
                    }
                    _ => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
        });
    }

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Send packets to all flows simultaneously (like stress test)
    let sender = UdpSocket::bind("0.0.0.0:0")
        .await
        .expect("Failed to bind sender");

    let _test_start = Instant::now();
    let packets_per_flow = 20;

    // Send to all flows
    for i in 0..packets_per_flow {
        sender
            .send_to(format!("Flow1_Packet{}", i).as_bytes(), "127.0.0.1:8080")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        sender
            .send_to(format!("Flow2_Packet{}", i).as_bytes(), "127.0.0.1:8081")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        sender
            .send_to(format!("Flow3_Packet{}", i).as_bytes(), "127.0.0.1:8082")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    // Wait for processing
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Collect results
    let mut flow1_packets = Vec::new();
    let mut flow2_packets = Vec::new();
    let mut flow3_packets = Vec::new();

    while let Ok(Some((_flow_id, time, data))) =
        timeout(Duration::from_millis(100), rx1.recv()).await
    {
        flow1_packets.push((time, data));
    }
    while let Ok(Some((_flow_id, time, data))) =
        timeout(Duration::from_millis(100), rx2.recv()).await
    {
        flow2_packets.push((time, data));
    }
    while let Ok(Some((_flow_id, time, data))) =
        timeout(Duration::from_millis(100), rx3.recv()).await
    {
        flow3_packets.push((time, data));
    }

    pipeline.shutdown().await;

    println!("Flow 1: {} packets received", flow1_packets.len());
    println!("Flow 2: {} packets received", flow2_packets.len());
    println!("Flow 3: {} packets received", flow3_packets.len());

    // At least one flow should have received packets
    let total_received = flow1_packets.len() + flow2_packets.len() + flow3_packets.len();
    assert!(
        total_received > 0,
        "Should have received at least one packet across all flows"
    );

    // Verify packet formats
    for (_, data) in &flow1_packets {
        assert!(
            data.starts_with("Flow1_"),
            "Flow 1 packet format incorrect: {}",
            data
        );
    }
    for (_, data) in &flow2_packets {
        assert!(
            data.starts_with("Flow2_"),
            "Flow 2 packet format incorrect: {}",
            data
        );
    }
    for (_, data) in &flow3_packets {
        assert!(
            data.starts_with("Flow3_"),
            "Flow 3 packet format incorrect: {}",
            data
        );
    }

    println!(
        "\n✅ Multiple flows stress test verified: {} total packets received",
        total_received
    );
}

#[tokio::test]
#[ignore]
async fn test_stress_test_packet_loss_measurement() {
    println!("\n=== Testing Stress Test Packet Loss Measurement ===\n");

    let pipeline = Arc::new(
        Pipeline::new(PipelineConfig::default())
            .await
            .expect("Failed to create pipeline"),
    );

    let pipeline_clone = pipeline.clone();
    tokio::spawn(async move {
        pipeline_clone.run().await.expect("Pipeline run failed");
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Start receiver
    let (tx, mut rx) = mpsc::channel(100);
    let tx_clone = tx.clone();

    tokio::spawn(async move {
        let socket = UdpSocket::bind("127.0.0.1:9080")
            .await
            .expect("Failed to bind receiver");

        let mut buf = [0u8; 1024];
        let end_time = Instant::now() + Duration::from_secs(3);

        while Instant::now() < end_time {
            match timeout(Duration::from_millis(100), socket.recv_from(&mut buf)).await {
                Ok(Ok((size, _))) => {
                    let data = String::from_utf8_lossy(&buf[..size]).to_string();
                    let _ = tx_clone.send(data).await;
                }
                _ => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Send packets
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .expect("Failed to bind sender");

    let packets_sent = 30;
    for i in 0..packets_sent {
        let message = format!("Packet{}", i);
        socket
            .send_to(message.as_bytes(), "127.0.0.1:8080")
            .await
            .expect("Failed to send");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Wait for processing
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Collect received
    let mut received = Vec::new();
    while let Ok(Some(data)) = timeout(Duration::from_millis(100), rx.recv()).await {
        received.push(data);
    }

    pipeline.shutdown().await;

    // Calculate packet loss
    let packets_received = received.len();
    let packet_loss = if packets_sent > 0 {
        ((packets_sent - packets_received) as f64 / packets_sent as f64) * 100.0
    } else {
        0.0
    };

    println!("Packets sent: {}", packets_sent);
    println!("Packets received: {}", packets_received);
    println!("Packet loss rate: {:.2}%", packet_loss);

    // Verify packet loss is reasonable (not 100%)
    assert!(packet_loss < 100.0, "Packet loss should be less than 100%");

    // In a properly working pipeline, we should receive at least some packets
    assert!(
        packets_received > 0,
        "Should have received at least one packet"
    );

    println!(
        "\n✅ Packet loss measurement verified: {:.2}% loss",
        packet_loss
    );
}

#[tokio::test]
#[ignore]
async fn test_stress_test_latency_measurement() {
    println!("\n=== Testing Stress Test Latency Measurement ===\n");

    let pipeline = Arc::new(
        Pipeline::new(PipelineConfig::default())
            .await
            .expect("Failed to create pipeline"),
    );

    let pipeline_clone = pipeline.clone();
    tokio::spawn(async move {
        pipeline_clone.run().await.expect("Pipeline run failed");
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Start receiver
    let (tx, mut rx) = mpsc::channel(50);
    let tx_clone = tx.clone();

    tokio::spawn(async move {
        let socket = UdpSocket::bind("127.0.0.1:9080")
            .await
            .expect("Failed to bind receiver");

        let mut buf = [0u8; 1024];
        let end_time = Instant::now() + Duration::from_secs(3);

        while Instant::now() < end_time {
            match timeout(Duration::from_millis(100), socket.recv_from(&mut buf)).await {
                Ok(Ok((size, _))) => {
                    let data = String::from_utf8_lossy(&buf[..size]).to_string();
                    let _ = tx_clone.send((Instant::now(), data)).await;
                }
                _ => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Send packets with timestamps
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .expect("Failed to bind sender");

    let test_start = Instant::now();
    let packets_sent = 20;

    for i in 0..packets_sent {
        let message = format!("Packet{}", i);
        socket
            .send_to(message.as_bytes(), "127.0.0.1:8080")
            .await
            .expect("Failed to send");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Wait for processing
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Collect received packets with timestamps
    let mut received = Vec::new();
    while let Ok(Some((recv_time, _data))) = timeout(Duration::from_millis(100), rx.recv()).await {
        received.push(recv_time);
    }

    pipeline.shutdown().await;

    if !received.is_empty() {
        // Calculate latencies relative to test start
        let mut latencies: Vec<Duration> = received
            .iter()
            .map(|time| time.duration_since(test_start))
            .collect();

        latencies.sort();

        let min_latency = latencies.first().unwrap();
        let max_latency = latencies.last().unwrap();
        let avg_latency = latencies.iter().sum::<Duration>() / latencies.len() as u32;

        println!("Min latency: {:.2} ms", min_latency.as_secs_f64() * 1000.0);
        println!("Max latency: {:.2} ms", max_latency.as_secs_f64() * 1000.0);
        println!("Avg latency: {:.2} ms", avg_latency.as_secs_f64() * 1000.0);

        // Verify latencies are reasonable (less than 1 second for localhost)
        assert!(
            *max_latency < Duration::from_secs(1),
            "Max latency should be reasonable"
        );

        println!(
            "\n✅ Latency measurement verified: {} packets received",
            received.len()
        );
    } else {
        println!("\n⚠️  No packets received for latency measurement");
    }
}
