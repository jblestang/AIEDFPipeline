# Performance Bottlenecks (Current State)

The pipeline has been refactored to minimise shared locks and to confine metrics work to the best-effort core. The remaining bottlenecks centre on packet allocation, bounded-channel sizing, and transport throughput. This document lists the dominant issues, their impact, and recommended actions.

## Top Bottlenecks

### 1. Ingress Packet Allocation
- **Where**: `IngressDRRScheduler::process_sockets` → `Vec::from(&local_buf[..size])`.
- **Impact**: One heap allocation per packet. Under stress (randomised payload sizes up to 1400 bytes) allocator contention can spike P95/P99 latencies for the High class.
- **Mitigation ideas**: Introduce a buffer pool (`bytes::Bytes`, slab, or freelist), reuse preallocated buffers, or switch to `bytes::BytesMut::freeze()` with reference counting.

### 2. Channel Saturation (Ingress → EDF)
- **Where**: Bounded crossbeam channel sized by `queues.ingress_to_edf[priority]` (default 16).
- **Impact**: When the EDF core is saturated, ingress drops accumulate rapidly. The GUI/metrics already surface these drops.
- **Mitigation ideas**: Increase the channel size for critical priorities, reduce ingress quantum for low-priority flows, or raise EDF throughput (e.g., lower simulated work for larger packets).

### 3. EDF Busy-Spin Cost
- **Where**: `EDFScheduler::process_next` busy-waits for the simulated processing time (0.2–0.4 ms).
- **Impact**: Occupies the dedicated EDF core completely. If other high-priority work needs the same core this becomes a hard bottleneck.
- **Mitigation ideas**: Keep EDF pinned to an exclusive core; if consolidation is necessary, switch to adaptive spinning (spin briefly, then `sleep`) or use a timer wheel.

### 4. Channel Saturation (EDF → Egress)
- **Where**: Bounded crossbeam channel sized by `queues.edf_to_egress[priority]` (default 16).
- **Impact**: When the egress runtime cannot keep up—or the OS network stack stalls—EDF output drops increase, wasting already-processed packets.
- **Mitigation ideas**: Increase channel capacity, retune egress DRR quantum, or enlarge OS UDP socket buffers.

### 5. UDP Kernel Buffers
- **Where**: `UdpSocket::send_to` inside `EgressDRRScheduler::process_queues`.
- **Impact**: Kernel buffer exhaustion currently results in silent packet loss (no metric). This can cascade into EDF output drops if the buffer remains full.
- **Mitigation ideas**: Surface send errors in metrics, configure larger socket buffers, or introduce backpressure by temporarily slowing EDF processing when repeated send failures occur.

## Secondary Considerations
- **Ingress mutex scope**: Ingress still acquires the state mutex twice per serviced packet (before/after reading). Combining these into a single critical section would shave a few hundred nanoseconds per packet and reduce jitter.
- **Stack buffer size**: The 1024-byte ingress buffer forces multiple syscalls for payloads >1024 bytes. Increasing it to 1500 bytes may help once buffer pooling is in place.
- **Best Effort work**: Ensure additional background tasks (logging, telemetry) stay on the best-effort core so they cannot delay ingress/EDF threads.

## Monitoring Checklist
1. Watch ingress and EDF drop counters in the GUI; consistent non-zero values signal saturation.
2. Track queue occupancy metrics (`queue*_occupancy` vs `queue*_capacity`) to spot headroom shortages.
3. Measure allocator statistics (e.g., `MallocStackLogging`, `perf mem`, `heaptrack`) when experimenting with alternative buffer strategies.
4. Observe CPU utilisation per core; EDF should run at ~100% on its pinned core during heavy load, ingress and egress should remain below saturation.

## Tuning Playbook
1. **Reduce drops first**: Adjust `PipelineConfig` queue sizes and quantums starting with the highest-priority class.
2. **Optimise allocations**: Implement buffer reuse and re-run the stress test to confirm improvements in P95/P99.
3. **Introduce transport counters**: Add metrics for `send_to` failures so the GUI reflects kernel-level drops.
4. **Iterate**: Bench with `cargo bench` or capture latency histograms from the GUI to validate each change.

Keeping these levers in mind ensures new scheduling strategies can be plugged in without regressing latency guarantees for high-priority flows.
