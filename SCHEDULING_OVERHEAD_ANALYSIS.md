# Scheduling Overhead Analysis

## Overview
This document analyzes the cost of scheduling operations vs. packet processing time in the MC-EDF scheduler.

## Processing Time Simulation

The `processing_duration()` function simulates packet processing based on payload size:
- Base processing time: **0.05ms (50µs)** for packets <= 200 bytes
- Additional time: up to **0.1ms (100µs)** for larger packets (linear interpolation)
- Maximum processing time: **0.15ms (150µs)** for 1500-byte packets
- Typical range: **50-150µs per packet**

## Scheduling Operations Breakdown

### 1. Reception Phase (Lines 710-750)
**Operations:**
- `try_recv()` on HIGH queue: ~50-100ns (lock-free channel operation)
- `try_recv()` on MEDIUM/LOW/BEST_EFFORT queues: ~50-100ns each
- Lock acquisition for heap push: ~10-50ns (parking_lot::Mutex)
- Heap push operation: ~100-500ns (BinaryHeap insertion, O(log n))
- Lock release: ~10ns

**Total per packet (best case - HIGH arrives):**
- ~200-700ns (0.2-0.7µs)

**Total per packet (worst case - batching multiple packets):**
- ~500-2000ns (0.5-2µs) for 5-10 packets

### 2. Selection Phase (Lines 752-759)
**Operations:**
- Lock acquisition: ~10-50ns
- Heap pop (min extraction): ~100-500ns (O(log n))
- Lock release: ~10ns

**Total:**
- ~120-560ns (0.12-0.56µs)

### 3. Preemption Check (Lines 762-798) - Only for MEDIUM/LOW
**Operations:**
- Lock acquisition for latency tracker: ~10-50ns
- Read average latency: ~10ns
- Lock release: ~10ns
- `recv_timeout()` call: **Variable** - blocks for up to `avg_processing_time * 1.1`
  - If HIGH arrives: ~50-100ns (immediate wake)
  - If timeout: blocks for ~55-220µs (55µs default, up to 220µs if avg=200µs)
- Lock acquisition for heap push (if HIGH arrives): ~10-50ns
- Heap push (2 operations): ~200-1000ns
- Task clone: ~50-200ns (depends on packet size)
- Lock release: ~10ns

**Total (HIGH arrives during wait):**
- ~330-1410ns (0.33-1.41µs) + wake latency (~1-5µs)

**Total (timeout expires):**
- ~30-70ns (just the check) + timeout duration (55-165µs, since avg processing = 50-150µs)

### 4. Latency Recording (Lines 815-820) - Only for HIGH
**Operations:**
- Lock acquisition: ~10-50ns
- EMA calculation: ~20-50ns
- Lock release: ~10ns

**Total:**
- ~40-110ns (0.04-0.11µs)

### 5. Output Forwarding (Lines 822-825)
**Operations:**
- `try_send()` on channel: ~50-100ns (lock-free)
- Atomic increment (if drop): ~10ns

**Total:**
- ~50-110ns (0.05-0.11µs)

## Total Scheduling Overhead

### HIGH Priority Packets (No Preemption Wait)
**Best case (single packet, no batching):**
- Reception: 0.2-0.7µs
- Selection: 0.12-0.56µs
- Latency recording: 0.04-0.11µs
- Output: 0.05-0.11µs
- **Total: ~0.41-1.48µs**

**Worst case (batching 10 packets):**
- Reception: 0.5-2µs (amortized per packet)
- Selection: 0.12-0.56µs
- Latency recording: 0.04-0.11µs
- Output: 0.05-0.11µs
- **Total: ~0.71-2.78µs per packet**

### MEDIUM/LOW Priority Packets (With Preemption Wait)
**Best case (HIGH arrives immediately):**
- Reception: 0.2-0.7µs
- Selection: 0.12-0.56µs
- Preemption check: 0.33-1.41µs + 1-5µs wake latency
- Output: 0.05-0.11µs
- **Total: ~0.7-7.78µs** (if preempted, restart loop)

**Worst case (timeout expires, no HIGH):**
- Reception: 0.2-0.7µs
- Selection: 0.12-0.56µs
- Preemption check: 0.03-0.07µs + 55-165µs timeout (avg_processing * 1.1)
- Output: 0.05-0.11µs
- **Total: ~55.4-166.44µs** (dominated by timeout)

## Overhead vs. Processing Time

### Processing Time Range
- Minimum: **50µs** (small packets <= 200 bytes)
- Maximum: **150µs** (large packets = 1500 bytes)
- Average: **~100µs** (typical packet size)

### Scheduling Overhead Percentage

**HIGH Priority (no timeout):**
- Best case: 0.41µs / 50µs = **0.8%**
- Worst case: 2.78µs / 50µs = **5.6%**
- Average: ~1µs / 100µs = **1.0%**

**MEDIUM/LOW Priority (with timeout):**
- Best case (preempted): 7.78µs / 50µs = **15.6%**
- Worst case (timeout): 166.44µs / 50µs = **333%** (timeout = 165µs for 150µs processing!)
- Average (timeout = 55µs, processing = 100µs): 55.4µs / 100µs = **55.4%**

## Key Observations

1. **Scheduling overhead is minimal for HIGH priority** (~0.8-5.6%, typically ~1%)
   - Lock-free channels are very fast
   - Heap operations are O(log n) but n is typically small (<100)
   - No timeout wait for HIGH

2. **Preemption timeout dominates MEDIUM/LOW overhead**
   - Default timeout (55µs) is ~55-110% of processing time (50-100µs)
   - If avg processing = 150µs, timeout = 165µs (110% overhead!)
   - This is intentional: allows HIGH to preempt, but adds significant latency to MEDIUM/LOW
   - **The timeout can be 2-3x the actual processing time!**

3. **Heap operations are efficient**
   - BinaryHeap with small n (<100) is very fast
   - Lock contention minimized by short critical sections

4. **Channel operations are lock-free and fast**
   - `try_recv()`: ~50-100ns
   - `recv_timeout()`: fast if data available, blocks if not

## Recommendations

1. **For HIGH priority**: Current overhead is excellent (~1%, max 5.6%)
   - No changes needed

2. **For MEDIUM/LOW priority**: Consider reducing timeout
   - Current: `avg_processing_time * 1.1` (55-165µs)
   - Could reduce to `avg_processing_time * 0.3-0.5` (15-75µs) to reduce overhead
   - Trade-off: Less opportunity for HIGH preemption, but overhead drops from 55% to ~15-30%
   - **Current timeout is very conservative** - HIGH has plenty of time to arrive

3. **Monitor heap size**
   - If heap grows >100 tasks, consider batching or multiple heaps
   - Current O(log n) becomes noticeable at n>1000

4. **Consider adaptive timeout**
   - Reduce timeout when queue depth is high
   - Increase timeout when queue depth is low (more slack for HIGH)

## Measurement Approach

To measure actual overhead, add timing instrumentation:

```rust
let schedule_start = Instant::now();
// ... scheduling operations ...
let schedule_overhead = schedule_start.elapsed();
// Compare with processing_time
```

This would provide real-world measurements on your specific hardware.

