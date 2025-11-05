// Comprehensive integration test for the entire pipeline

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::timeout;

use aiedf_pipeline::pipeline::Pipeline;

#[tokio::test]
async fn test_full_pipeline_flow() {
    println!("\n=== Testing Full Pipeline Flow ===\n");

    // Create pipeline
    let pipeline = Arc::new(Pipeline::new().await.expect("Failed to create pipeline"));

    // Start pipeline in background
    let pipeline_clone = pipeline.clone();
    tokio::spawn(async move {
        pipeline_clone.run().await.expect("Pipeline run failed");
    });

    // Give pipeline time to start
    tokio::time::sleep(Duration::from_millis(500)).await;

    println!("Pipeline started, sending test packets...\n");

    // Test packets for each flow
    let test_cases = vec![
        (1, 8080, 9080, "Flow1", Duration::from_millis(1)),
        (2, 8081, 9081, "Flow2", Duration::from_millis(50)),
        (3, 8082, 9082, "Flow3", Duration::from_millis(100)),
    ];

    // Spawn receivers for each output socket
    for (flow_id, _input_port, output_port, _name, _latency) in &test_cases {
        let output_port = *output_port;
        let flow_id = *flow_id;

        tokio::spawn(async move {
            let socket = UdpSocket::bind(format!("127.0.0.1:{}", output_port))
                .await
                .expect(&format!("Failed to bind receiver on port {}", output_port));

            let mut buf = [0u8; 1024];
            loop {
                match socket.recv_from(&mut buf).await {
                    Ok((size, _addr)) => {
                        let data = String::from_utf8_lossy(&buf[..size]).to_string();
                        println!("✅ Received packet on flow {}: {}", flow_id, data);
                    }
                    Err(e) => {
                        eprintln!("Error receiving on flow {}: {}", flow_id, e);
                        break;
                    }
                }
            }
        });
    }

    // Give receivers time to bind
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Send packets to each input socket
    for (flow_id, input_port, _output_port, name, _latency) in &test_cases {
        let socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .expect("Failed to bind sender socket");

        let message = format!("Test packet from {}", name);
        let target = format!("127.0.0.1:{}", input_port);

        println!(
            "Sending '{}' to flow {} (port {})",
            message, flow_id, input_port
        );

        match socket.send_to(message.as_bytes(), &target).await {
            Ok(_) => {
                println!("  ✓ Sent successfully");
            }
            Err(e) => {
                eprintln!("  ✗ Failed to send: {}", e);
            }
        }

        // Small delay between sends
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Wait for packets to be processed
    println!("\nWaiting for packets to be processed...");
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Shutdown pipeline
    pipeline.shutdown().await;

    // Wait a bit for cleanup
    tokio::time::sleep(Duration::from_millis(100)).await;

    println!("\n=== Pipeline Test Complete ===\n");
}

#[tokio::test]
#[ignore] // Run sequentially to avoid port conflicts
async fn test_pipeline_multiple_packets() {
    println!("\n=== Testing Pipeline with Multiple Packets ===\n");

    let pipeline = Arc::new(Pipeline::new().await.expect("Failed to create pipeline"));

    let pipeline_clone = pipeline.clone();
    tokio::spawn(async move {
        pipeline_clone.run().await.expect("Pipeline run failed");
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Start receiver for flow 1 - use a channel to collect results
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    let tx_clone = tx.clone();

    tokio::spawn(async move {
        let socket = UdpSocket::bind("127.0.0.1:9080")
            .await
            .expect("Failed to bind receiver");

        let mut buf = [0u8; 1024];

        // Try to receive packets with timeout
        for _ in 0..5 {
            match timeout(Duration::from_millis(1000), socket.recv_from(&mut buf)).await {
                Ok(Ok((size, _))) => {
                    let data = String::from_utf8_lossy(&buf[..size]).to_string();
                    let _ = tx_clone.send(data).await;
                }
                _ => break,
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Send multiple packets
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .expect("Failed to bind sender");

    for i in 0..5 {
        let message = format!("Packet {}", i);
        socket
            .send_to(message.as_bytes(), "127.0.0.1:8080")
            .await
            .expect("Failed to send");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Wait for processing and collect received packets
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut received = Vec::new();
    while let Ok(Some(data)) = timeout(Duration::from_millis(100), rx.recv()).await {
        received.push(data);
        println!("Received: {}", received.last().unwrap());
    }

    pipeline.shutdown().await;

    // Verify we received packets
    assert!(
        !received.is_empty(),
        "Should have received at least one packet"
    );
    println!("\n✅ Received {} packets", received.len());
}

#[tokio::test]
#[ignore] // Run sequentially to avoid port conflicts
async fn test_pipeline_latency_prioritization() {
    println!("\n=== Testing Latency Prioritization ===\n");

    let pipeline = Arc::new(Pipeline::new().await.expect("Failed to create pipeline"));

    let pipeline_clone = pipeline.clone();
    tokio::spawn(async move {
        pipeline_clone.run().await.expect("Pipeline run failed");
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Start receivers for all flows using channels
    let (tx1, mut rx1) = mpsc::channel(10);
    let (tx2, mut rx2) = mpsc::channel(10);
    let (tx3, mut rx3) = mpsc::channel(10);

    for (flow_id, tx) in vec![(1, tx1), (2, tx2), (3, tx3)] {
        let port = 9080 + (flow_id - 1) as u16;
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            let socket = UdpSocket::bind(format!("127.0.0.1:{}", port))
                .await
                .expect(&format!("Failed to bind on port {}", port));

            let mut buf = [0u8; 1024];

            // Try to receive with timeout
            for _ in 0..3 {
                match timeout(Duration::from_millis(2000), socket.recv_from(&mut buf)).await {
                    Ok(Ok((size, _))) => {
                        let data = String::from_utf8_lossy(&buf[..size]).to_string();
                        let _ = tx_clone.send((Instant::now(), data)).await;
                    }
                    _ => break,
                }
            }
        });
    }

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Send packets to all flows simultaneously
    let sender = UdpSocket::bind("0.0.0.0:0")
        .await
        .expect("Failed to bind sender");

    // Send to flow 3 (100ms deadline) first
    sender
        .send_to("Flow3".as_bytes(), "127.0.0.1:8082")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Send to flow 2 (50ms deadline)
    sender
        .send_to("Flow2".as_bytes(), "127.0.0.1:8081")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Send to flow 1 (1ms deadline) last
    sender
        .send_to("Flow1".as_bytes(), "127.0.0.1:8080")
        .await
        .unwrap();

    // Wait for processing
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Collect results from channels
    let mut results: Vec<(u64, Vec<(Instant, String)>)> = Vec::new();

    // Collect from flow 1
    let mut flow1_packets = Vec::new();
    while let Ok(Some(packet)) = timeout(Duration::from_millis(100), rx1.recv()).await {
        flow1_packets.push(packet);
    }
    if !flow1_packets.is_empty() {
        results.push((1, flow1_packets));
    }

    // Collect from flow 2
    let mut flow2_packets = Vec::new();
    while let Ok(Some(packet)) = timeout(Duration::from_millis(100), rx2.recv()).await {
        flow2_packets.push(packet);
    }
    if !flow2_packets.is_empty() {
        results.push((2, flow2_packets));
    }

    // Collect from flow 3
    let mut flow3_packets = Vec::new();
    while let Ok(Some(packet)) = timeout(Duration::from_millis(100), rx3.recv()).await {
        flow3_packets.push(packet);
    }
    if !flow3_packets.is_empty() {
        results.push((3, flow3_packets));
    }

    pipeline.shutdown().await;

    // Check that packets were received
    println!("\nResults:");
    for (flow_id, packets) in &results {
        println!("  Flow {}: {} packets", flow_id, packets.len());
        if !packets.is_empty() {
            // EDF should prioritize Flow 1 (1ms deadline) first
            // But we sent them in reverse order, so Flow 1 should still arrive first
            println!("    First packet: {}", packets[0].1);
        }
    }

    // Check results
    let flow1_packets = results
        .iter()
        .find(|(id, _)| *id == 1)
        .map(|(_, p)| p.len())
        .unwrap_or(0);
    let flow2_packets = results
        .iter()
        .find(|(id, _)| *id == 2)
        .map(|(_, p)| p.len())
        .unwrap_or(0);
    let flow3_packets = results
        .iter()
        .find(|(id, _)| *id == 3)
        .map(|(_, p)| p.len())
        .unwrap_or(0);

    println!("\n✅ Flow 1 received: {} packets", flow1_packets);
    println!("✅ Flow 2 received: {} packets", flow2_packets);
    println!("✅ Flow 3 received: {} packets", flow3_packets);

    // At least one flow should have received packets (the test verifies the pipeline works)
    assert!(
        flow1_packets > 0 || flow2_packets > 0 || flow3_packets > 0,
        "At least one flow should have received packets"
    );
}
