# Packet Drop Analysis

## Overview
This document analyzes all locations in the pipeline where packets can be dropped, leading to missing packets in the output.

## Pipeline Flow

```
UDP Socket (recv_from)
  ↓
IngressDRRScheduler::process_sockets()
  ↓ [DROP POINT 1: Queue Full]
Priority Queue (HIGH/MEDIUM/LOW) - bounded(128)
  ↓
EDFScheduler::process_next()
  ↓ [DROP POINT 2: Heap Full]
EDF BinaryHeap - MAX_HEAP_SIZE = 128
  ↓ [DROP POINT 3: Queue Full]
Priority Queue (HIGH/MEDIUM/LOW) - bounded(128)
  ↓
EgressDRRScheduler::process_queues()
  ↓
UDP Socket (send_to)
```

## Identified Drop Points

### 1. **IngressDRR → Priority Queue: Silent Drop on Full Queue** ⚠️ CRITICAL

**Location**: `src/ingress_drr.rs:230-235`
```rust
if tx.try_send(packet).is_ok() {
    packets_read += 1;
} else {
    // Queue full, move to next flow
    break;  // PACKET IS DROPPED HERE - NO ERROR, NO RETRY
}
```

**Problem**:
- Queue capacity: **128 packets** (bounded channel)
- When queue is full, `try_send()` returns `Err(Full)`
- Packet is **silently dropped** - no retry, no error logging
- Flow moves to next flow, losing the packet forever

**Impact**:
- **High packet loss** when input rate exceeds processing rate
- No visibility into dropped packets (no metrics)
- Affects all priorities equally (no prioritization on drop)

**When it happens**:
- EDF scheduler is slow (heap operations take time)
- Burst of packets arrives faster than EDF can process
- Queue fills up (128 packets), new packets are dropped

**Recommendation**:
- Add drop counter/metrics to track dropped packets
- Consider backpressure: if HIGH priority queue is full, drop LOW priority packets from other queues
- Or increase queue size (but increases memory usage)
- Or use blocking send with timeout (but adds latency)

### 2. **EDF Scheduler: Heap Full - Drops Packets** ⚠️ CRITICAL

**Location**: `src/edf_scheduler.rs:166-189`
```rust
if tasks.len() >= MAX_HEAP_SIZE {
    // Drop a packet to make room
    let _ = tasks.pop();  // PACKET IS DROPPED HERE
}
tasks.push(task);
```

**Problem**:
- Heap capacity: **128 packets** (MAX_HEAP_SIZE = 128)
- When heap is full, **one packet is dropped** for each new packet inserted
- Drop logic tries to prioritize (drops LOW priority if inserting HIGH), but still drops packets
- Packets are dropped **silently** - no metrics, no logging

**Impact**:
- **High packet loss** when burst arrives
- Packets with later deadlines are dropped (heap is min-heap by deadline)
- No visibility into dropped packets

**When it happens**:
- Burst of packets arrives faster than EgressDRR can consume
- Heap fills up (128 packets)
- New packets cause old packets to be dropped

**Recommendation**:
- Add drop counter/metrics to track dropped packets per priority
- Increase heap size (but increases memory and processing time)
- Implement smarter drop policy (e.g., drop packets that already missed deadline)
- Or use backpressure: if heap is full, don't accept more packets from input queues

### 3. **EDF → Output Queue: Silent Drop on Full Queue** ⚠️ CRITICAL

**Location**: `src/edf_scheduler.rs:205, 208, 211`
```rust
match packet.priority {
    Priority::High => {
        let _ = self.high_priority_output_tx.try_send(packet);  // DROPS IF FULL
    }
    Priority::Medium => {
        let _ = self.medium_priority_output_tx.try_send(packet);  // DROPS IF FULL
    }
    Priority::Low => {
        let _ = self.low_priority_output_tx.try_send(packet);  // DROPS IF FULL
    }
}
```

**Problem**:
- Queue capacity: **128 packets** per priority (bounded channel)
- When queue is full, `try_send()` returns `Err(Full)`
- Packet is **silently dropped** - no retry, no error
- Result is ignored (`let _ = ...`)

**Impact**:
- **High packet loss** when EgressDRR is slow
- No visibility into dropped packets
- Processed packets (through EDF) are lost

**When it happens**:
- EgressDRR is slow (UDP send is slow, or async runtime is busy)
- Output queues fill up (128 packets each)
- EDF processes packets but can't send them - packets are dropped

**Recommendation**:
- Add drop counter/metrics to track dropped packets
- Consider backpressure: if output queue is full, stop processing from input queues
- Or increase queue size
- Or use blocking send with timeout

### 4. **UDP Socket Receive: WouldBlock (Not a Drop, But Missed)** ⚠️ LOW IMPACT

**Location**: `src/ingress_drr.rs:237-240`
```rust
Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
    // No packet available for this flow, move to next flow
    break;
}
```

**Problem**:
- Non-blocking socket: if no packet is available, returns `WouldBlock`
- Not a drop, but packet might be in OS buffer
- If we don't check again soon, packet might be delayed

**Impact**: Low (packet is still in OS buffer, will be read later)

**Recommendation**: Already handled correctly (non-blocking, will retry in next iteration)

### 5. **UDP Socket Send: Async I/O (Potential Blocking)** ⚠️ MEDIUM IMPACT

**Location**: `src/egress_drr.rs:82`
```rust
let _ = socket.send_to(&packet.data, *target_addr).await;
```

**Problem**:
- Async I/O: if socket buffer is full, `await` can block
- If it blocks, EgressDRR can't process more packets
- Output queues fill up, causing drops at EDF → Output Queue

**Impact**: Indirect - causes queue to fill up, leading to drops

**Recommendation**: Use non-blocking I/O like IngressDRR

## Summary of Drop Points

| Location | Queue/Buffer | Capacity | Drop Behavior | Visibility |
|----------|--------------|----------|--------------|------------|
| IngressDRR → Input Queue | bounded(128) | 128 | Silent drop, no retry | ❌ None |
| EDF Heap | MAX_HEAP_SIZE | 128 | Drops oldest/lowest priority | ❌ None |
| EDF → Output Queue | bounded(128) | 128 | Silent drop, no retry | ❌ None |

## Root Causes

1. **Bounded Queues (128 capacity)**: Too small for burst traffic
2. **No Backpressure**: When queue is full, packets are dropped instead of backpressuring
3. **No Drop Metrics**: Can't see how many packets are being dropped
4. **No Retry Logic**: Packets are dropped immediately, no retry mechanism
5. **Heap Size Limit (128)**: Too small for burst traffic

## Recommended Fixes

### 1. **Add Drop Counters and Metrics** (IMMEDIATE)

Track dropped packets at each drop point:
- IngressDRR: Count packets dropped when `try_send()` fails
- EDF: Count packets dropped when heap is full
- EDF Output: Count packets dropped when `try_send()` fails

**Implementation**:
```rust
// In IngressDRR
if tx.try_send(packet).is_ok() {
    packets_read += 1;
} else {
    // Queue full - track drop
    drop_counter.fetch_add(1, Ordering::Relaxed);
    break;
}
```

### 2. **Increase Queue Capacities** (QUICK FIX)

**Current**: 128 packets per queue
**Recommended**: 512 or 1024 packets per queue

**Trade-off**: More memory usage, but reduces drops

### 3. **Implement Backpressure** (MEDIUM TERM)

Instead of dropping packets, backpressure upstream:
- If output queue is full, stop processing from input queues
- If EDF heap is full, stop accepting packets from input queues
- This prevents drops but can cause input queues to fill up

**Implementation**:
```rust
// In EDF, before processing input queues
if tasks.len() >= MAX_HEAP_SIZE {
    // Backpressure: don't process input queues
    return None;
}
```

### 4. **Smarter Drop Policy** (MEDIUM TERM)

When dropping is necessary, be smarter:
- Drop packets that already missed deadline
- Drop LOW priority packets when inserting HIGH priority
- Drop packets with latest deadlines first

**Current**: Already partially implemented (drops LOW when inserting HIGH)

### 5. **Use Unbounded Queues** (NOT RECOMMENDED)

**Problem**: Unbounded queues can cause memory exhaustion
**Better**: Use larger bounded queues with monitoring

### 6. **Add Retry Logic** (OPTIONAL)

Instead of dropping immediately, retry with exponential backoff:
- Retry `try_send()` a few times before dropping
- Adds latency but reduces drops

## Testing Strategy

1. **Stress Test**: Send packets at high rate, measure drop rate
2. **Burst Test**: Send burst of packets, verify all are processed
3. **Queue Monitoring**: Track queue occupancy over time
4. **Drop Metrics**: Verify drop counters are accurate

## Monitoring

Add metrics for:
- Queue occupancy (already exists)
- Drop counts per drop point
- Drop rate (drops per second)
- Queue full events

## Example Drop Scenarios

### Scenario 1: Burst Traffic
1. Burst of 200 packets arrives
2. IngressDRR sends 128 to input queue (72 dropped)
3. EDF processes 128, sends to output queue
4. EgressDRR sends 128 to UDP (all processed)
5. **Result**: 72 packets lost

### Scenario 2: Slow EgressDRR
1. EgressDRR is slow (UDP send is slow)
2. Output queues fill up (128 packets each)
3. EDF processes packets but can't send (drops)
4. Input queues fill up (128 packets each)
5. IngressDRR can't send (drops)
6. **Result**: Cascading packet loss

### Scenario 3: EDF Heap Full
1. Burst arrives, EDF heap fills up (128 packets)
2. New packets cause old packets to be dropped
3. If burst is 200 packets, 72 packets are dropped from heap
4. **Result**: 72 packets lost in EDF

## References

- [crossbeam-channel Documentation](https://docs.rs/crossbeam-channel/)
- [Backpressure Pattern](https://en.wikipedia.org/wiki/Backpressure_(software))

