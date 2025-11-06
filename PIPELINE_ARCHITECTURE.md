# Pipeline Architecture Diagram

## Complete Data Flow

```
┌─────────────────────────────────────────────────────────────────────────────────────┐
│                           UDP INPUT SOCKETS (Multiple Flows)                         │
│                                                                                     │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐                            │
│  │ Flow 1 (HIGH)│  │ Flow 2 (MED) │  │ Flow 3 (LOW) │                            │
│  │ 127.0.0.1:8080│  │ 127.0.0.1:8081│  │ 127.0.0.1:8082│                            │
│  │ Latency: 5ms │  │ Latency: 50ms │  │ Latency: 100ms│                            │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘                            │
│         │                 │                 │                                       │
│         ▼                 ▼                 ▼                                       │
│  ┌───────────────────────────────────────────────────────────────────────────┐      │
│  │                      INGRESS DRR SCHEDULER                                │      │
│  │  (Reads from UDP Sockets, DRR schedules, routes to priority queues)       │      │
│  │  - Single Thread/Task for all sockets                                     │      │
│  │  - DRR Scheduling based on Packet Quantum                                 │      │
│  │  - Quantum Values: HIGH=32768, MEDIUM=4096, LOW=1024                      │      │
│  │  - Outputs to 3 separate priority queues for EDF                          │      │
│  └───────────────┬───────────────┬───────────────┘                            │
│                  │               │               │                              │
│                  ▼               ▼               ▼                              │
│         ┌───────────────────────────────────────────────────────────────────┐      │
│         │                     EDF SCHEDULER                                 │      │
│         │  (Earliest Deadline First - K-way Merge from 3 Input Queues)       │      │
│         │  - BinaryHeap (Max Size: 128)                                     │      │
│         │  - K-way Merge: Compares first element from each priority queue   │      │
│         │  - Inputs: HIGH, MEDIUM, LOW Priority Queues (from Ingress DRR)   │      │
│         │  - Outputs: HIGH, MEDIUM, LOW Priority Queues (to Egress DRR)     │      │
│         │  - Lock: Single mutex, batch operations                           │      │
│         └───────────────┬───────────────┬───────────────┘                    │
│                  │               │               │                              │
│                  ▼               ▼               ▼                              │
│  ┌───────────────────────────────────────────────────────────────────────────┐      │
│  │                      EGRESS DRR SCHEDULER                                │      │
│  │  (Reads from 3 Priority Queues, DRR schedules, writes to UDP Sockets)     │      │
│  │  - Processes: HIGH→MEDIUM→LOW                                             │      │
│  │  - Outputs to UDP Output Sockets                                          │      │
│  └───────────────┬───────────────┬───────────────┘                            │
│                  │               │               │                              │
│                  ▼               ▼               ▼                              │
│         ┌───────────────────────────────────────────────────────────────────┐      │
│         │                     METRICS COLLECTOR                             │      │
│         │  - Record latency, track deadlines, calculate stats               │      │
│         └───────────────┬───────────────────────────────┘                    │
│                         │                                                      │
│                         ▼                                                      │
│         ┌───────────────────────────────────────────────────────────────────┐      │
│         │                     UDP OUTPUT SOCKETS                            │      │
│         │  (Pre-computed SocketAddr)                                        │      │
│         │                                                                   │      │
│  ┌──────▼──────┐  ┌──────▼──────┐  ┌──────▼──────┐                      │
│  │ Flow 1 (HIGH)│  │ Flow 2 (MED)│  │ Flow 3 (LOW)│                      │
│  │ 127.0.0.1:9080│  │ 127.0.0.1:9081│  │ 127.0.0.1:9082│                      │
│  └─────────────┘  └─────────────┘  └─────────────┘                      │
│                                                                           │
└───────────────────────────────────────────────────────────────────────────┘
```

## Thread Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                         MAIN THREAD (Tokio Runtime)                          │
│                                                                             │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │  UDP INPUT HANDLERS (Async Tasks - One per socket)                   │  │
│  │  - All flows: Send to Input DRR (with priority routing)              │  │
│  │  - Priority mapping: Flow 1→HIGH, Flow 2→MEDIUM, Flow 3→LOW         │  │
│  │  - Priority: Real-time (high)                                        │  │
│  └──────────────────────────────────────────────────────────────────────┘  │
│                                                                             │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │  INGRESS DRR PROCESSOR (Async Task)                                   │  │
│  │  - Reads from all UDP Input Sockets                                    │  │
│  │  - Performs DRR scheduling based on quantum                            │  │
│  │  - Routes packets to EDF's 3 input priority queues                    │  │
│  └──────────────────────────────────────────────────────────────────────┘  │
│                                                                             │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │  EDF PROCESSOR (std::thread)                                         │  │
│  │  - Thread: "EDF-Processor"                                          │  │
│  │  - Priority: 2 (High)                                               │  │
│  │  - Reads from EDF's 3 input priority queues                         │  │
│  │  - Performs EDF scheduling using K-way merge                          │  │
│  │  - Routes packets to EDF's 3 output priority queues                 │  │
│  └──────────────────────────────────────────────────────────────────────┘  │
│                                                                             │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │  EGRESS DRR PROCESSOR (Async Task)                                   │  │
│  │  - Reads from EDF's 3 output priority queues                        │  │
│  │  - Performs DRR scheduling (HIGH→MEDIUM→LOW)                        │  │
│  │  - Writes packets to UDP Output Sockets                             │  │
│  │  - Records metrics after processing                                 │  │
│  └──────────────────────────────────────────────────────────────────────┘  │
│                                                                             │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │  STATISTICS THREAD (std::thread)                                      │  │
│  │  - Thread: "Statistics-Thread"                                       │  │
│  │  - Priority: 1 (Lower - Utility)                                    │  │
│  │  - Periodically sends metrics to GUI (every 100ms)                  │  │
│  └──────────────────────────────────────────────────────────────────────┘  │
│                                                                             │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │  METRICS SERVER (TCP - Port 9999)                                    │  │
│  │  - Broadcasts MetricsSnapshot to GUI clients                         │  │
│  │  - JSON serialization (microsecond precision)                       │  │
│  └──────────────────────────────────────────────────────────────────────┘  │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────────────┐
│                         GUI CLIENT (Separate Binary)                         │
│                                                                             │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │  TCP Client → Connects to Metrics Server (127.0.0.1:9999)          │  │
│  │  - Receives MetricsSnapshot JSON                                     │  │
│  │  - Updates real-time plots (latency, percentiles, std dev)           │  │
│  │  - Displays statistics table                                         │  │
│  │  - Logarithmic Y-axis with linear values                             │  │
│  └──────────────────────────────────────────────────────────────────────┘  │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

## Component Details

### 1. **UDP Input Sockets**
- **Flow 1 (HIGH)**: `127.0.0.1:8080` - 5ms deadline
- **Flow 2 (MEDIUM)**: `127.0.0.1:8081` - 50ms deadline
- **Flow 3 (LOW)**: `127.0.0.1:8082` - 100ms deadline
- **Threading**: Async tasks (Tokio)
- **Priority Mapping**: Flow ID → Priority (1→HIGH, 2→MEDIUM, 3→LOW)

### 2. **Ingress DRR Scheduler**
- **Type**: Deficit Round Robin with Priority Queues
- **Function**: Reads from all input UDP sockets, applies DRR scheduling, and routes to EDF's input queues.
- **Quantum Values**: 
  - HIGH priority (Flow 1): 32768
  - MEDIUM priority (Flow 2): 4096
  - LOW priority (Flow 3): 1024
- **Internal State**: `flow_states` (deficit, quantum), `active_flows`, `current_flow_index`
- **Output**: 3 `crossbeam_channel::Sender<Packet>` (HIGH, MEDIUM, LOW) to EDF
- **Thread**: Single Async Task
- **Socket Handling**: Uses `std::net::UdpSocket` set to non-blocking, `recv_from` for non-blocking reads.

### 3. **EDF Scheduler**
- **Type**: Earliest Deadline First (BinaryHeap) with K-way Merge
- **Max Size**: 128 tasks
- **Input**: 3 `Arc<crossbeam_channel::Receiver<Packet>>` (HIGH, MEDIUM, LOW) from Ingress DRR
- **Output**: 3 `crossbeam_channel::Sender<Packet>` (HIGH, MEDIUM, LOW) to Egress DRR
- **K-way Merge Algorithm**:
  - Buffers first packet from each priority queue
  - Compares deadlines without removing from channels
  - Inserts only the packet with earliest deadline into heap
  - Refills buffer for consumed queue
  - Maintains deadline order regardless of priority queue source
- **Lock Optimization**: Single mutex, batch operations
- **Thread**: "EDF-Processor" (Priority 2)

### 4. **Egress DRR Scheduler**
- **Type**: Deficit Round Robin with Priority Queues
- **Function**: Reads from EDF's output queues, applies DRR scheduling, and writes to UDP output sockets.
- **Input**: 3 `crossbeam_channel::Receiver<Packet>` (HIGH, MEDIUM, LOW) from EDF
- **Output**: Writes directly to `Arc<UdpSocket>`
- **Priority Levels**: HIGH (Flow 1), MEDIUM (Flow 2), LOW (Flow 3)
- **Quantum Values**:
  - HIGH priority: 32768
  - MEDIUM priority: 4096
  - LOW priority: 1024
- **Processing Order**: HIGH → MEDIUM → LOW (strict priority)
- **Socket Handling**: `output_sockets: Arc<Mutex<HashMap<u64, (Arc<UdpSocket>, SocketAddr)>>>`

### 5. **UDP Output Sockets**
- **Flow 1**: `127.0.0.1:9080`
- **Flow 2**: `127.0.0.1:9081`
- **Flow 3**: `127.0.0.1:9082`
- **Optimization**: Pre-computed `SocketAddr` (no string formatting)
- **Threading**: Async task (Tokio)

### 6. **Metrics Collector**
- **Storage**: `HashMap<u64, Metrics>` (parking_lot::Mutex)
- **Data**: Latencies (microseconds), packet counts, deadline misses
- **Statistics**: P50, P95, P99, P999, P100, std dev, avg, min, max
- **Caching**: Percentiles cached to avoid repeated sorting
- **History**: Last 1000 samples per flow

### 7. **Metrics Server**
- **Protocol**: TCP
- **Port**: 9999
- **Format**: JSON (MetricsSnapshot)
- **Precision**: Microseconds (sub-millisecond)
- **Broadcast**: All connected GUI clients

### 8. **GUI Client**
- **Binary**: `cargo run --bin gui`
- **Framework**: egui
- **Features**: 
  - Real-time latency plots (log Y-axis)
  - Statistics table (P50, P95, P99, P999, P100, std dev)
  - History: 1000 points per flow
  - Expected max latency overlay

## Data Flow Paths

### Flow 1 (HIGH Priority - 5ms):
```
UDP Input (8080) → Ingress DRR [DRR Scheduling] → EDF [HIGH Queue] → Egress DRR [HIGH Queue] → UDP Output (9080)
```
**Priority**: HIGH - Processed first in Ingress DRR, EDF input, and Egress DRR schedulers

### Flow 2 (MEDIUM Priority - 50ms):
```
UDP Input (8081) → Ingress DRR [DRR Scheduling] → EDF [MEDIUM Queue] → Egress DRR [MEDIUM Queue] → UDP Output (9081)
```
**Priority**: MEDIUM - Processed after HIGH priority packets

### Flow 3 (LOW Priority - 100ms):
```
UDP Input (8082) → Ingress DRR [DRR Scheduling] → EDF [LOW Queue] → Egress DRR [LOW Queue] → UDP Output (9082)
```
**Priority**: LOW - Processed after HIGH and MEDIUM priority packets

**Note**: All flows now go through Ingress DRR. Priority queues ensure HIGH priority packets are always processed first. EDF scheduler uses K-way merge to process packets by deadline regardless of priority queue source.

## Performance Optimizations Applied

1. ✅ **Lock-free queue operations** (removed mutex wrappers)
2. ✅ **Lock-free running flag** (AtomicBool)
3. ✅ **Pre-computed SocketAddr** (zero string allocations)
4. ✅ **DRR lock simplification** (single DRRState lock)
5. ✅ **EDF K-way merge** (compare first elements from all queues before removing)
6. ✅ **EDF heap size reduction** (128 instead of 1000 for better memory efficiency)
7. ✅ **DRR buffer optimization** (single pass instead of two)
8. ✅ **Memory allocation** (Vec::from() for exact capacity)
9. ✅ **Priority-based DRR scheduling** (3 separate priority queues: HIGH, MEDIUM, LOW)
10. ✅ **Ingress/Egress DRR separation** (dedicated schedulers for input and output)

## Priority System

### Packet Priority Levels
- **HIGH Priority**: Flow 1 (5ms deadline) - Always processed first
- **MEDIUM Priority**: Flow 2 (50ms deadline) - Processed after HIGH
- **LOW Priority**: Flow 3 (100ms deadline) - Processed after HIGH and MEDIUM

### DRR Priority Queue Processing
Both Input and Output DRR schedulers maintain 3 separate priority queues:
1. **HIGH Priority Queue**: Processed first, contains Flow 1 packets
2. **MEDIUM Priority Queue**: Processed only when HIGH queue is empty
3. **LOW Priority Queue**: Processed only when HIGH and MEDIUM queues are empty

Within each priority queue, packets are scheduled using DRR (Deficit Round Robin) with deadline-aware selection (earliest deadline first).

## Thread Priorities (macOS QoS)

- **Priority 2+**: `QOS_CLASS_USER_INITIATED` (EDF, Input DRR)
- **Priority 1**: `QOS_CLASS_UTILITY` (Statistics)
- **Default**: `QOS_CLASS_BACKGROUND` (others)

## CPU Affinity (Linux only)

- Bound to 3 CPU cores for better cache locality

