# AIEDF Pipeline

A high-performance scheduler pipeline in Rust implementing Deficit Round Robin (DRR) and Earliest Deadline First (EDF) schedulers for network flow processing with real-time latency monitoring.

## Architecture

The pipeline consists of three main stages:

1. **Input DRR Scheduler**: Handles incoming UDP network flows with different latency requirements (1ms, 50ms, 100ms)
2. **EDF Scheduler**: Processes data packets with respect to their expected deadlines
3. **Output DRR Scheduler**: Schedules outgoing UDP packets

### Components

- **DRR Scheduler** (`src/drr_scheduler.rs`): Implements Deficit Round Robin scheduling algorithm for fair bandwidth allocation across multiple flows
- **EDF Scheduler** (`src/edf_scheduler.rs`): Implements Earliest Deadline First scheduling for deadline-aware processing
- **Queues** (`src/queue.rs`): Lock-free queues connecting scheduler stages
- **Metrics** (`src/metrics.rs`): Real-time latency and deadline miss tracking
- **Pipeline** (`src/pipeline.rs`): Main pipeline orchestration with thread priorities and CPU affinity
- **GUI** (`src/gui.rs`): Real-time monitoring dashboard using egui

## Features

- ✅ DRR scheduling for incoming and outgoing network flows
- ✅ EDF scheduling for deadline-aware data processing
- ✅ Queue-based communication between schedulers
- ✅ Thread priorities and CPU affinity (3 cores)
- ✅ Real-time metrics collection
- ✅ egui-based monitoring GUI
- ✅ Comprehensive unit tests
- ✅ Performance benchmarks

## Requirements

- Rust 1.70+ (2021 edition)
- **Supported Platforms:**
  - **Linux**: Full support including CPU affinity (3 cores) and thread priorities
  - **macOS**: Full support (GUI runs on main thread, CPU affinity not available, thread naming supported)

## Building

```bash
cargo build --release
```

## Running

### Run Pipeline (without GUI)

```bash
cargo run --release
```

This runs the pipeline and processes packets. You can monitor it via console output.

### Run Pipeline with GUI

The GUI is a separate binary that connects to the running pipeline via TCP:

**Terminal 1 - Start the pipeline:**
```bash
cargo run --release
```

The pipeline will start a metrics server on `127.0.0.1:9999`.

**Terminal 2 - Start the GUI:**
```bash
cargo run --release --bin gui
```

The GUI connects to the metrics server and displays:
- Packet counts per flow
- Latency statistics (average, min, max, percentiles)
- HDRHistogram percentiles (P50, P95, P99, P99.9)
- Deadline misses
- Real-time latency visualization

**Note**: The GUI is a standalone client that connects to the running pipeline. You must run the pipeline first before starting the GUI.

The pipeline will:
1. Listen for UDP packets on multiple input sockets:
   - `127.0.0.1:8080` (1ms latency budget, flow_id: 1)
   - `127.0.0.1:8081` (50ms latency budget, flow_id: 2)
   - `127.0.0.1:8082` (100ms latency budget, flow_id: 3)
2. Send processed packets to corresponding output sockets:
   - Flow 1 → `127.0.0.1:9080`
   - Flow 2 → `127.0.0.1:9081`
   - Flow 3 → `127.0.0.1:9082`
3. Open a GUI window for real-time monitoring

## Testing

Run unit tests:
```bash
cargo test
```

Run integration tests:
```bash
cargo test --test integration_test
```

## Benchmarking

Run performance benchmarks:
```bash
cargo bench
```

## Packet Format

UDP packets are sent directly to the appropriate input socket. The flow_id and latency budget are determined by which socket receives the packet:

- **Socket `127.0.0.1:8080`**: Flow ID 1, 1ms latency budget
- **Socket `127.0.0.1:8081`**: Flow ID 2, 50ms latency budget
- **Socket `127.0.0.1:8082`**: Flow ID 3, 100ms latency budget

No packet header is required - the data is sent directly as the payload. The socket it arrives on determines its flow characteristics.

## Example Usage

### Sending Test Packets

You can use `netcat` or a simple UDP client to send test packets to different input sockets:

```bash
# Send packet to flow 1 socket (1ms latency budget)
echo -n "Hello" | nc -u 127.0.0.1 8080

# Send packet to flow 2 socket (50ms latency budget)
echo -n "World" | nc -u 127.0.0.1 8081

# Send packet to flow 3 socket (100ms latency budget)
echo -n "Test" | nc -u 127.0.0.1 8082
```

Packets will be processed and sent to the corresponding output sockets:
- Flow 1 packets → `127.0.0.1:9080`
- Flow 2 packets → `127.0.0.1:9081`
- Flow 3 packets → `127.0.0.1:9082`

### Receiving Packets

You can listen on the output sockets to receive processed packets:

```bash
# Listen for flow 1 output
nc -u -l 9080

# Listen for flow 2 output
nc -u -l 9081

# Listen for flow 3 output
nc -u -l 9082
```

### Monitoring

The GUI displays:
- Real-time latency metrics per flow
- Packet counts
- Minimum, maximum, and average latencies
- Deadline miss counts
- Visual latency distribution

## Architecture Details

### Thread Configuration

- **Input Threads**: Multiple threads (one per input socket) receive UDP packets and schedule via input DRR
- **EDF Processing Thread**: High-priority thread for deadline-aware processing
- **Output DRR Thread**: Processes packets from EDF and routes to queue3
- **Output Thread**: Sends processed packets to appropriate output sockets based on flow_id
- **GUI**: Runs on main thread (required on macOS, works on Linux too)

### Multi-Socket Architecture

The pipeline supports multiple UDP sockets for input and output, each with its own latency budget:

- **Input Sockets**: Each socket listens on a different port and has a configured latency budget
- **Flow Routing**: Packets are tagged with flow_id based on which input socket received them
- **Output Routing**: Processed packets are routed to the appropriate output socket based on their flow_id
- **Latency Budgets**: Each socket pair (input/output) has a matching latency budget for end-to-end latency guarantees

### Platform-Specific Features

#### Linux
- **CPU Affinity**: Configured to use 3 CPU cores (cores 0, 1, 2)
- **Thread Priorities**: EDF processing thread runs with higher priority (SCHED_FIFO, priority 2)
- **Note**: CPU affinity may require root privileges

#### macOS
- **CPU Affinity**: Not available (macOS manages CPU allocation differently)
- **Thread Priorities**: Thread naming supported; priority managed via Quality of Service
- **GUI**: Must run on main thread (macOS requirement)

Both platforms support:
- Full UDP networking functionality
- Real-time metrics collection
- GUI monitoring dashboard
- All scheduler features

## Performance Considerations

- Uses lock-free channels (crossbeam-channel) for inter-thread communication
- Parking lot mutexes for low-contention locking
- Efficient packet processing with minimal allocations
- Real-time metrics with minimal overhead

## License

This project is provided as-is for educational and research purposes.

