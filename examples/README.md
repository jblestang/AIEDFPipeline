# Using the AIEDF Pipeline

This directory contains examples for sending and receiving data through the pipeline.

## Quick Start

### 1. Start the Pipeline

```bash
# From the project root
cargo run --release
```

The pipeline will:
- Listen on input sockets: `127.0.0.1:8080`, `127.0.0.1:8081`, `127.0.0.1:8082`
- Send to output sockets: `127.0.0.1:9080`, `127.0.0.1:9081`, `127.0.0.1:9082`
- Open a GUI window showing latency metrics

### 2. Send Data (Method 1: Using netcat)

```bash
# Send to Flow 1 (1ms latency budget)
echo -n "Hello" | nc -u 127.0.0.1 8080

# Send to Flow 2 (50ms latency budget)
echo -n "World" | nc -u 127.0.0.1 8081

# Send to Flow 3 (100ms latency budget)
echo -n "Test" | nc -u 127.0.0.1 8082
```

### 3. Receive Data (Method 1: Using netcat)

Open separate terminals for each output socket:

```bash
# Terminal 1: Receive Flow 1 output
nc -u -l 9080

# Terminal 2: Receive Flow 2 output
nc -u -l 9081

# Terminal 3: Receive Flow 3 output
nc -u -l 9082
```

### 4. Stress Test - Verify Latency Prioritization

**This is the most important test!** It verifies that the EDF scheduler correctly prioritizes packets with tighter deadlines.

```bash
# Make sure the pipeline is running first
cargo run --release

# In another terminal, run the stress test
cargo run --example stress_test
```

The stress test will:
- Send packets to all three flows simultaneously
- Verify that Flow 1 (1ms deadline) is processed first by EDF
- Measure packet arrival times and throughput
- Report on prioritization correctness

**Expected Result**: Flow 1 packets should arrive first, demonstrating correct EDF prioritization.

### 5. Send Data (Method 2: Using the shell script)

```bash
chmod +x examples/send_packets.sh
./examples/send_packets.sh
```

### 6. Send/Receive Data (Method 3: Using the Rust client)

```bash
# Build and run the test client
cd examples
cargo run --example test_client
```

## Socket Configuration

### Input Sockets (Send data here)

| Port | Flow ID | Latency Budget | Description |
|------|---------|----------------|-------------|
| 8080 | 1 | 1ms | Ultra-low latency |
| 8081 | 2 | 50ms | Medium latency |
| 8082 | 3 | 100ms | High latency |

### Output Sockets (Receive data here)

| Port | Flow ID | Latency Budget | Description |
|------|---------|----------------|-------------|
| 9080 | 1 | 1ms | Ultra-low latency |
| 9081 | 2 | 50ms | Medium latency |
| 9082 | 3 | 100ms | High latency |

## Packet Format

**No special format required!** Just send raw data:

- The socket you send to determines the flow_id and latency budget
- Data is passed through as-is (no header parsing)
- Any binary data can be sent

## Example Workflow

1. **Start the pipeline:**
   ```bash
   cargo run --release
   ```

2. **In Terminal 1, listen for Flow 1 output:**
   ```bash
   nc -u -l 9080
   ```

3. **In Terminal 2, send data to Flow 1:**
   ```bash
   echo -n "Hello Pipeline!" | nc -u 127.0.0.1 8080
   ```

4. **You should see the data appear in Terminal 1** (after processing through the pipeline)

## Testing Latency Prioritization

The most critical test is the stress test, which verifies EDF prioritization:

```bash
# Terminal 1: Start pipeline
cargo run --release

# Terminal 2: Run stress test
cargo run --example stress_test
```

The stress test sends packets to all flows simultaneously and verifies:
- ✅ Flow 1 (1ms deadline) packets are processed first
- ✅ Packet arrival order matches deadline priority
- ✅ Throughput and latency metrics

## Testing with Multiple Flows

You can test all three flows simultaneously:

```bash
# Terminal 1: Listen on Flow 1 output
nc -u -l 9080

# Terminal 2: Listen on Flow 2 output
nc -u -l 9081

# Terminal 3: Listen on Flow 3 output
nc -u -l 9082

# Terminal 4: Send to all flows
echo -n "Flow1" | nc -u 127.0.0.1 8080
echo -n "Flow2" | nc -u 127.0.0.1 8081
echo -n "Flow3" | nc -u 127.0.0.1 8082
```

## Monitoring

The GUI window shows:
- Real-time latency metrics per flow
- Packet counts
- Min/max/average latencies
- Deadline misses
- Visual latency distribution

## Troubleshooting

1. **Port already in use**: Make sure no other application is using ports 8080-8082 or 9080-9082
2. **No data received**: Check that the pipeline is running and the GUI is showing activity
3. **Connection refused**: Ensure the pipeline has started successfully
4. **Stress test shows no prioritization**: Check that EDF scheduler is working correctly

## Advanced: Custom Socket Configuration

To change socket configurations, edit `src/pipeline.rs` in the `Pipeline::new(PipelineConfig)` method:

```rust
let input_sockets = vec![
    SocketConfig {
        address: "127.0.0.1".to_string(),
        port: 8080,
        latency_budget: Duration::from_millis(1),
        flow_id: 1,
    },
    // Add more sockets...
];
```
