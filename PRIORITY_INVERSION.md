# Priority Inversion Analysis

## Overview
Priority inversion occurs when a high-priority task is blocked by a lower-priority task holding a shared resource (typically a mutex), and an intermediate-priority task preempts the lower-priority task, preventing it from releasing the resource.

## Current Thread Priorities

1. **EDF Processor Thread**: Priority 2 (HIGH) - Processes packets with deadline scheduling
2. **Statistics Thread**: Priority 1 (LOW) - Computes and sends metrics periodically
3. **IngressDRR**: Async task (no explicit priority) - Reads from UDP sockets
4. **EgressDRR**: Async task (no explicit priority) - Writes to UDP sockets and records metrics

## Identified Priority Inversion Risks

### 1. **Metrics Mutex Contention** ⚠️ HIGH RISK

**Location**: `src/metrics.rs` - `MetricsCollector::metrics: Arc<Mutex<HashMap<u64, Metrics>>>`

**Problem**:
- **Statistics Thread (Priority 1, LOW)** calls `send_current_metrics()` which:
  - Locks the metrics mutex
  - Iterates over all flows
  - Computes percentiles (P50, P95, P99, P999) which involves **sorting** latency data
  - This can take significant time (especially with 10 seconds of history)
  
- **EgressDRR (Async task, no explicit priority)** calls `record_packet()` which:
  - Locks the same metrics mutex
  - Records latency for each packet
  - This happens in the hot path (every packet processed)

**Priority Inversion Scenario**:
1. Statistics thread (low priority) acquires metrics mutex
2. Statistics thread starts computing percentiles (sorting, can take milliseconds)
3. EgressDRR tries to record a packet → blocked waiting for mutex
4. If EgressDRR is processing high-priority packets, they are delayed
5. The delay propagates through the pipeline

**Impact**: 
- High-priority packets can be delayed by low-priority statistics computation
- Packet processing latency increases unpredictably
- Worst-case latency can exceed deadlines

### 2. **parking_lot::Mutex Does NOT Support Priority Inheritance**

**Problem**:
- All mutexes use `parking_lot::Mutex` which is fast but **does not support priority inheritance**
- Priority inheritance would temporarily boost the priority of a low-priority thread holding a lock that a high-priority thread needs
- Without priority inheritance, high-priority threads can be blocked indefinitely by low-priority threads

**Affected Mutexes**:
- `IngressDRRScheduler::state: Arc<Mutex<IngressDRRState>>`
- `EDFScheduler::tasks: Arc<Mutex<BinaryHeap<EDFTask>>>`
- `EgressDRRScheduler::output_sockets: Arc<Mutex<HashMap<...>>>`
- `MetricsCollector::metrics: Arc<Mutex<HashMap<u64, Metrics>>>`

### 3. **EDF Scheduler Mutex** ⚠️ MEDIUM RISK

**Location**: `src/edf_scheduler.rs` - `tasks: Arc<Mutex<BinaryHeap<EDFTask>>>`

**Problem**:
- EDF thread (Priority 2, HIGH) locks this mutex in `process_next()`
- Lock is held while:
  - Collecting packets from input queues
  - Inserting into binary heap
  - Popping from heap
  - Routing to output queues

**Potential Issue**:
- If IngressDRR (async, no explicit priority) somehow needed to access this, it would be blocked
- Currently, IngressDRR doesn't access EDF's heap directly, so this is lower risk
- However, if the lock is held for too long, it could delay packet processing

### 4. **IngressDRR State Mutex** ✅ LOW RISK

**Location**: `src/ingress_drr.rs` - `state: Arc<Mutex<IngressDRRState>>`

**Status**: Only accessed by IngressDRR itself, no cross-thread contention.

## Recommended Solutions

### 1. **Separate Metrics Recording from Statistics Computation** (RECOMMENDED)

**Approach**: Use a lock-free channel to decouple metrics recording from statistics computation.

```rust
// In MetricsCollector
metrics_events: Arc<crossbeam_channel::Sender<MetricsEvent>>, // Lock-free
metrics: Arc<Mutex<HashMap<u64, Metrics>>>, // Only accessed by statistics thread

// EgressDRR sends events (lock-free)
metrics_collector.send_event(MetricsEvent::RecordLatency { flow_id, latency, ... });

// Statistics thread processes events and updates metrics
```

**Benefits**:
- EgressDRR never blocks on mutex (lock-free send)
- Statistics thread processes events at its own pace
- Eliminates priority inversion in the hot path

### 2. **Minimize Lock Hold Time in Statistics Thread**

**Current Issue**: `send_current_metrics()` holds the lock while computing all percentiles.

**Solution**: 
- Clone metrics data quickly (minimal lock time)
- Compute percentiles outside the lock
- Only lock when sending the snapshot

```rust
pub fn send_current_metrics(&self, ...) {
    // Quick clone (minimal lock time)
    let metrics_snapshot = {
        let metrics = self.metrics.lock();
        metrics.clone() // Clone outside lock
    };
    
    // Compute statistics outside lock (no blocking)
    for (flow_id, m) in metrics_snapshot.iter() {
        // Compute percentiles here (no lock held)
    }
}
```

### 3. **Use Priority-Inheriting Mutexes** (Platform-Specific)

**Linux**: Use `pthread_mutex_setprioceiling()` or `PTHREAD_PRIO_INHERIT`
**macOS**: Use `pthread_mutexattr_setprotocol()` with `PTHREAD_PRIO_INHERIT`

**Challenge**: `parking_lot::Mutex` doesn't support this. Would need:
- Custom mutex wrapper
- Or use `std::sync::Mutex` with platform-specific extensions
- Or use a real-time mutex library

### 4. **Lock-Free Data Structures for Metrics**

**Approach**: Use atomic operations and lock-free data structures for metrics recording.

**Example**: 
- Use `crossbeam::SkipList` or similar for latency storage
- Use atomic counters for packet counts
- Use lock-free queues for metrics events

**Trade-off**: More complex implementation, but eliminates mutex contention entirely.

### 5. **Reduce Statistics Computation Frequency**

**Current**: Statistics computed every 100ms

**Solution**: 
- Increase interval to 500ms or 1s
- Reduces lock contention frequency
- Still provides adequate GUI update rate

## Immediate Action Items

1. ✅ **Document the issue** (this file)
2. ⚠️ **Implement Solution #2** (minimize lock hold time) - Quick win
3. 🔄 **Consider Solution #1** (lock-free channel) - Best long-term solution
4. 📊 **Add metrics for lock contention** - Monitor the problem
5. 🧪 **Stress test with high packet rates** - Verify fixes

## Testing Priority Inversion

To verify priority inversion is occurring:

1. **Monitor lock wait times**: Add timing around mutex acquisitions
2. **Measure packet latency during statistics computation**: Should see spikes
3. **Profile with high packet rates**: Statistics thread should not delay packet processing
4. **Use real-time analysis tools**: `perf`, `ftrace`, or similar

## References

- [Priority Inversion - GeeksforGeeks](https://www.geeksforgeeks.org/priority-inversion-what-the-heck/)
- [parking_lot Mutex Documentation](https://docs.rs/parking_lot/latest/parking_lot/type.Mutex.html)
- [Priority Inheritance Protocol](https://en.wikipedia.org/wiki/Priority_inheritance)

