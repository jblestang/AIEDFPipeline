# Hot Path Analysis

## Overview
Packets traverse the following hot path:

```
UDP recv (non-blocking, per-priority sockets)
  ↓
IngressDRRScheduler::process_sockets (Tokio current-thread runtime)
  ↓
PriorityTable<Sender<Packet>> bounded channel
  ↓
EDFScheduler::process_next (dedicated CPU core, busy-spin)
  ↓
PriorityTable<Sender<Packet>> bounded channel
  ↓
EgressDRRScheduler::process_queues (Tokio current-thread runtime)
  ↓
UDP send (Tokio)
```

Each step was tuned to minimise contention, yet several operations still dominate latency or CPU consumption. This document highlights the remaining hot spots and outlines possible optimisations.

## Stage-by-Stage Notes

### Ingress DRR (`IngressDRRScheduler::process_sockets`)
- **Single state mutex**: All DRR book-keeping (active flows, deficit counters, socket metadata) lives behind one `parking_lot::Mutex`. The lock is acquired briefly to bump a flow’s deficit and again to store the updated value after reading packets. Under high packet rates this still introduces two lock/unlock pairs per serviced packet.
- **Heap allocation per packet**: `Vec::from(&local_buf[..size])` allocates for every incoming packet. For typical payloads (0–1400 bytes) this is the dominant per-packet cost and can explain latency spikes when the allocator contends.
- **Small stack buffer**: The local read buffer is 1024 bytes. Larger payloads rely on multiple reads, increasing per-packet overhead. The stress harness currently randomises payload length up to 1400 bytes, so some extra copies occur.
- **Tokio `yield_now` fallback**: When no packets are available the loop yields, avoiding a tight spin but adding context switches if packets arrive immediately after yielding.

### EDF Processor (`EDFScheduler::process_next`)
- **Mutex scope**: The `pending` table is protected by a `parking_lot::Mutex`. The lock is held long enough to pull one packet per priority and select the earliest deadline. Because the table stores at most one entry per class, the critical section is short and scales with the number of priorities, not queue length.
- **Busy-spin during simulated work**: Size-aware processing time is emulated with `std::hint::spin_loop()`. This keeps latency predictable but ties up an entire core for the duration (0.2–0.4 ms). If we ever co-locate other work on the EDF core we will need an alternative (e.g., `park_timeout` or timer-based sleeping).
- **Channel backpressure**: EDF forwards packets with `try_send`. Failures immediately translate into drop counters, so the processor never blocks on the downstream queue.

### Egress DRR (`EgressDRRScheduler::process_queues`)
- **Socket map clone**: The scheduler clones its socket map once before entering the loop, so steady-state packet handling does not lock shared state.
- **Metrics emission**: Calling `metrics_collector.record_packet` sends a lock-free event; the background statistics thread drains those events on the best-effort core. No contention has been observed in the hot path because metrics processing never touches the collector mutex.
- **UDP send**: `send_to` awaits the kernel. Short stalls are expected when the kernel transmit buffer fills. Errors are ignored, equating to implicit drops.

## Current Bottlenecks
| Component | Cost | Details | Suggested Investigation |
|-----------|------|---------|-------------------------|
| Ingress allocation | HIGH | Heap allocation for every packet (`Vec::from`). | Adopt an arena/Slab, leverage `bytes::Bytes`, or recycle buffers. |
| Ingress locking | MEDIUM | Two mutex acquisitions per serviced packet. | Inline both updates under a single lock guard or move deficit to atomics. |
| EDF busy-spin | MEDIUM | Processing time emulation consumes 100% of core while active. | Replace with calibrated sleep or hybrid spin/sleep after threshold. |
| UDP send backpressure | MEDIUM | Kernel buffer saturation silently drops packets. | Surface errors to metrics and consider enlarging socket buffers. |
| Stack buffer size | LOW | 1024-byte ingress buffer requires multiple reads for jumbo payloads. | Increase to max payload if memory footprint allows. |

## Recommendations
1. **Buffer pooling**: Reusing packet buffers will cut allocator churn and stabilise ingress latency. The stress workload with 0–1400 byte payloads benefits directly.
2. **Lock scope reduction**: Combine the deficit increment/decrement into a single critical section per flow iteration. Alternatively, keep deficit counters in atomics and snapshot them when needed for fairness logic.
3. **Expose transport drops**: Capture `send_to` errors and feed them into the Best Effort metrics stream to complete the drop picture.
4. **Adaptive spin**: If the EDF core needs sharing, replace pure spinning with an adaptive strategy (spin for <50 µs, then sleep/yield). For now it is acceptable because EDF owns an entire core.
5. **Benchmark alternate quanta**: Continue experimenting with `PipelineConfig` to identify quanta/capacity combinations that minimise drops without growing queues unnecessarily.

## Validation Checklist
- Run `cargo run --example stress_test` while monitoring the GUI drop legend to ensure ingress drops remain dominant (expected) and EDF drops appear only under sustained overload.
- Profile ingress using `perf` or `dtrace` to quantify allocation cost after any buffer pooling changes.
- Keep measuring P50/P95/P99 for the High class after each tuning iteration; regressions typically point to ingress locking or allocator contention.

