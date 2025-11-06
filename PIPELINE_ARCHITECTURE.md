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
│         │                 │                 │                                       │
│         │                 │                 │                                       │
│         │                 ▼                 ▼                                       │
│         │         ┌───────────────────────────────┐                                 │
│         │         │   INPUT DRR SCHEDULER        │                                 │
│         │         │   (Deficit Round Robin)      │                                 │
│         │         │   - Priority Queues:         │                                 │
│         │         │     • HIGH (Flow 1): Q 32768 │                                 │
│         │         │     • MEDIUM (Flow 2): Q 4096│                                 │
│         │         │     • LOW (Flow 3): Q 1024   │                                 │
│         │         │   - 3 Separate Buffers (1000)│                                 │
│         │         │   - Process: HIGH→MEDIUM→LOW │                                 │
│         │         └───────────────┬───────────────┘                                 │
│         │                         │                                                 │
│         │                         │                                                 │
│         │                         ▼                                                 │
│         │              ┌───────────────────┐                                       │
│         │              │     QUEUE 1       │                                       │
│         │              │  (Bounded: 128)   │                                       │
│         │              │  Lock-free (crossbeam)│                                   │
│         │              └─────────┬─────────┘                                       │
│         │                        │                                                 │
│         │                        │                                                 │
│         │                        ▼                                                 │
│         │              ┌───────────────────┐                                       │
│         │              │   EDF SCHEDULER   │                                       │
│         │              │ (Earliest Deadline)│                                     │
│         │              │  - BinaryHeap      │                                       │
│         │              │  - Max Size: 1000│                                       │
│         │              │  - Lock: Batch ops │                                       │
│         │              └─────────┬─────────┘                                       │
│         │                        │                                                 │
│         │                        │                                                 │
│         │                        ▼                                                 │
│         │              ┌───────────────────┐                                       │
│         │              │     QUEUE 2       │                                       │
│         │              │  (Bounded: 128)   │                                       │
│         │              │  Lock-free (crossbeam)│                                   │
│         │              └─────────┬─────────┘                                       │
│         │                        │                                                 │
│         │                        │                                                 │
│         │                        ▼                                                 │
│         │         ┌───────────────────────────────┐                               │
│         │         │   OUTPUT DRR SCHEDULER         │                               │
│         │         │   (Deficit Round Robin)         │                               │
│         │         │   - Priority Queues:            │                               │
│         │         │     • HIGH (Flow 1): Q 32768    │                               │
│         │         │     • MEDIUM (Flow 2): Q 4096   │                               │
│         │         │     • LOW (Flow 3): Q 1024      │                               │
│         │         │   - 3 Separate Buffers (1000)    │                               │
│         │         │   - Process: HIGH→MEDIUM→LOW    │                               │
│         │         └───────────────┬───────────────┘                               │
│         │                         │                                                 │
│         │                         │                                                 │
│         │                         ▼                                                 │
│         │              ┌───────────────────┐                                       │
│         │              │  METRICS COLLECTOR │                                       │
│         │              │  - Record latency │                                       │
│         │              │  - Track deadlines│                                       │
│         │              │  - Calculate stats │                                       │
│         │              └─────────┬─────────┘                                       │
│         │                        │                                                 │
│         │                        │                                                 │
│         │                        ▼                                                 │
│         │         ┌───────────────────────────────┐                                 │
│         │         │   UDP OUTPUT SOCKETS          │                                 │
│         │         │   (Pre-computed SocketAddr)    │                                 │
│         │         │                                │                                 │
│         │  ┌──────▼──────┐  ┌──────▼──────┐  ┌──────▼──────┐                      │
│         │  │ Flow 1 (HIGH)│  │ Flow 2 (MED)│  │ Flow 3 (LOW)│                      │
│         │  │ 127.0.0.1:9080│  │ 127.0.0.1:9081│  │ 127.0.0.1:9082│                      │
│         │  └─────────────┘  └─────────────┘  └─────────────┘                      │
│         │                                                                           │
└─────────┴───────────────────────────────────────────────────────────────────────────┘
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
│  │  INPUT DRR PROCESSOR (std::thread)                                  │  │
│  │  - Thread: "Input-DRR"                                               │  │
│  │  - Priority: 2 (High)                                                 │  │
│  │  - Processes priority queues: HIGH → MEDIUM → LOW                   │  │
│  │  - Reads from priority buffers → Sends to Queue1                    │  │
│  └──────────────────────────────────────────────────────────────────────┘  │
│                                                                             │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │  EDF PROCESSOR (std::thread)                                         │  │
│  │  - Thread: "EDF-Processor"                                          │  │
│  │  - Priority: 2 (High)                                               │  │
│  │  - Reads from Queue1 → EDF scheduling → Sends to Queue2            │  │
│  └──────────────────────────────────────────────────────────────────────┘  │
│                                                                             │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │  OUTPUT DRR + UDP SENDER (Async Task)                                │  │
│  │  - Reads from Queue2 → Output DRR → UDP send                        │  │
│  │  - Pre-computed SocketAddr (no string formatting)                    │  │
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

### 2. **Input DRR Scheduler**
- **Type**: Deficit Round Robin with Priority Queues
- **Priority Levels**: HIGH (Flow 1), MEDIUM (Flow 2), LOW (Flow 3)
- **Quantum Values**: 
  - HIGH priority: 32768
  - MEDIUM priority: 4096
  - LOW priority: 1024
- **Queues**: 3 separate priority buffers (each bounded to 1000 packets)
- **Processing Order**: HIGH → MEDIUM → LOW (strict priority)
- **Lock**: Single `DRRState` mutex (optimized)
- **Thread**: "Input-DRR" (Priority 2)

### 3. **Queue 1**
- **Type**: Bounded crossbeam channel (128 capacity)
- **Lock-free**: Direct receiver access (no mutex wrapper)
- **Operations**: `try_send()`, `try_recv()` - all lock-free

### 4. **EDF Scheduler**
- **Type**: Earliest Deadline First (BinaryHeap)
- **Max Size**: 1000 tasks
- **Lock Optimization**: Collect packets first, then lock heap once
- **Thread**: "EDF-Processor" (Priority 2)

### 5. **Queue 2**
- **Type**: Bounded crossbeam channel (128 capacity)
- **Lock-free**: Direct receiver access (no mutex wrapper)
- **Operations**: `try_send()`, `try_recv()` - all lock-free

### 6. **Output DRR Scheduler**
- **Type**: Deficit Round Robin with Priority Queues
- **Priority Levels**: HIGH (Flow 1), MEDIUM (Flow 2), LOW (Flow 3)
- **Quantum Values**: 
  - HIGH priority: 32768
  - MEDIUM priority: 4096
  - LOW priority: 1024
- **Queues**: 3 separate priority buffers (each bounded to 1000 packets)
- **Processing Order**: HIGH → MEDIUM → LOW (strict priority)
- **Lock**: Single `DRRState` mutex (optimized)

### 7. **UDP Output Sockets**
- **Flow 1**: `127.0.0.1:9080`
- **Flow 2**: `127.0.0.1:9081`
- **Flow 3**: `127.0.0.1:9082`
- **Optimization**: Pre-computed `SocketAddr` (no string formatting)
- **Threading**: Async task (Tokio)

### 8. **Metrics Collector**
- **Storage**: `HashMap<u64, Metrics>` (parking_lot::Mutex)
- **Data**: Latencies (microseconds), packet counts, deadline misses
- **Statistics**: P50, P95, P99, P999, P100, std dev, avg, min, max
- **Caching**: Percentiles cached to avoid repeated sorting
- **History**: Last 1000 samples per flow

### 9. **Metrics Server**
- **Protocol**: TCP
- **Port**: 9999
- **Format**: JSON (MetricsSnapshot)
- **Precision**: Microseconds (sub-millisecond)
- **Broadcast**: All connected GUI clients

### 10. **GUI Client**
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
UDP Input (8080) → Input DRR [HIGH Queue] → Queue1 → EDF → Queue2 → Output DRR [HIGH Queue] → UDP Output (9080)
```
**Priority**: HIGH - Processed first in both Input and Output DRR schedulers

### Flow 2 (MEDIUM Priority - 50ms):
```
UDP Input (8081) → Input DRR [MEDIUM Queue] → Queue1 → EDF → Queue2 → Output DRR [MEDIUM Queue] → UDP Output (9081)
```
**Priority**: MEDIUM - Processed after HIGH priority packets

### Flow 3 (LOW Priority - 100ms):
```
UDP Input (8082) → Input DRR [LOW Queue] → Queue1 → EDF → Queue2 → Output DRR [LOW Queue] → UDP Output (9082)
```
**Priority**: LOW - Processed after HIGH and MEDIUM priority packets

**Note**: All flows now go through Input DRR. Priority queues ensure HIGH priority packets are always processed first.

## Performance Optimizations Applied

1. ✅ **Lock-free queue operations** (removed mutex wrappers)
2. ✅ **Lock-free running flag** (AtomicBool)
3. ✅ **Pre-computed SocketAddr** (zero string allocations)
4. ✅ **DRR lock simplification** (single DRRState lock)
5. ✅ **EDF lock batching** (collect packets before locking)
6. ✅ **DRR buffer optimization** (single pass instead of two)
7. ✅ **Memory allocation** (Vec::from() for exact capacity)
8. ✅ **Priority-based DRR scheduling** (3 separate priority queues: HIGH, MEDIUM, LOW)

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

