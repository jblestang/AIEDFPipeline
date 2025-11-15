# Slow Paths Analysis - MC-EDF Elastic Scheduler

## Critical Issues (High Impact)

### 1. **Lock Contention on Heap Mutex** ⚠️ CRITICAL
**Location:** `edf_worker_loop` - Lines 582, 601, 618

**Problem:**
- All 3 EDF workers (HIGH, MEDIUM, LOW) share the same core but have separate heaps
- However, each worker locks its own heap mutex frequently:
  - Line 582: Lock for every incoming task
  - Line 601: Lock to pop task for processing
  - Line 618: Lock inside elasticity loop when HIGH arrives

**Impact:** 
- Under high load, workers may contend for CPU time slice, causing lock delays
- Lock/unlock overhead on every operation (even with `parking_lot`)

**Frequency:**
- Every packet: 2-3 lock operations minimum
- During elasticity: Up to 30 additional lock checks (150µs / 5µs)

**Recommendation:**
- Consider lock-free data structures for heaps (e.g., `crossbeam::SkipList` or atomics)
- Batch operations to reduce lock acquisition frequency
- Use `RwLock` if reads >> writes (but writes are frequent here)

---

### 2. **Double Lock on Distribution Stats** ⚠️ MODERATE
**Location:** `dispatcher_loop` - Lines 481-483 and 495-497

**Problem:**
```rust
let stats = distribution_stats.lock();  // Lock #1
let channel = stats.find_best_channel(...);
drop(stats);

// ... send task ...

let mut stats = distribution_stats.lock();  // Lock #2
stats.record_distribution(...);
```

**Impact:**
- Two separate lock acquisitions for each HIGH packet
- Lock contention if dispatcher runs frequently
- Unnecessary unlock/lock cycle

**Frequency:**
- Every HIGH priority packet (could be thousands per second)

**Recommendation:**
- Hold lock across both operations:
```rust
let channel = {
    let mut stats = distribution_stats.lock();
    let ch = stats.find_best_channel(...);
    stats.record_distribution(ch, ...);
    ch
};
```

---

### 3. **Task Cloning in Dispatcher** ⚠️ MODERATE
**Location:** `dispatcher_loop` - Lines 487-489

**Problem:**
```rust
let sent = match channel {
    0 => high_tx.send(task.clone()).is_ok(),  // Clone #1
    1 => medium_tx.send(task.clone()).is_ok(), // Clone #2
    2 => low_tx.send(task.clone()).is_ok(),    // Clone #3
    _ => false,
};
```

**Impact:**
- `task.clone()` clones entire `EDFTask` struct including `Packet`
- Cloning occurs even if send fails
- Memory allocation overhead

**Frequency:**
- Every HIGH priority packet distribution decision

**Recommendation:**
- Only clone after determining channel:
```rust
let channel = { /* ... */ };
let sent = match channel {
    0 => high_tx.send(task.clone()).is_ok(),
    // ...
};
```

---

### 4. **CPU Contention (All EDFs on Same Core)** ⚠️ HIGH
**Location:** `spawn_threads` - Line 332

**Problem:**
- HIGH, MEDIUM, LOW EDF workers + Re-orderer all run on `worker_cores[0]`
- 4 threads competing for same CPU core
- OS scheduler context switching overhead
- Cache invalidation between threads

**Impact:**
- HIGH priority may be delayed by MEDIUM/LOW processing
- Context switch overhead (~1-10µs per switch)
- Reduced cache locality

**Frequency:**
- Continuous contention during active periods

**Recommendation:**
- Consider pinning HIGH EDF to dedicated core
- Use thread priorities to favor HIGH worker
- Consider cooperative yielding instead of OS scheduling

---

### 5. **Elasticity Loop Overhead** ⚠️ MODERATE
**Location:** `edf_worker_loop` - Lines 608-630

**Problem:**
```rust
while wait_start.elapsed() < elasticity {  // 150µs window
    if last_check.elapsed() >= check_interval {  // Every 5µs
        if let Ok(new_task) = input.try_recv() {
            // Lock heap, push 2 tasks, unlock
            let mut heap_guard = heap.lock();
            heap_guard.push(new_task);
            heap_guard.push(task.clone());  // Clone here too!
            // ...
        }
    }
    std::hint::spin_loop();
}
```

**Impact:**
- Up to 30 iterations (150µs / 5µs) per MEDIUM/LOW packet
- Lock acquisition on every HIGH arrival during elasticity
- Task cloning on every preemption
- Spin loop CPU usage

**Frequency:**
- Every MEDIUM/LOW packet that goes through elasticity (most of them)

**Recommendation:**
- Reduce check interval only if HIGH traffic is high
- Consider adaptive elasticity based on HIGH arrival rate
- Batch elasticity checks with other operations

---

## Moderate Issues (Medium Impact)

### 6. **Heap Lock Held During Push Loop**
**Location:** `edf_worker_loop` - Lines 579-597

**Problem:**
- Lock is acquired and released for each task in the receive loop
- Multiple lock/unlock cycles if multiple tasks arrive

**Recommendation:**
- Batch pushes:
```rust
let mut tasks = Vec::new();
loop {
    match input.try_recv() {
        Ok(task) => tasks.push(task),
        Err(_) => break,
    }
}
if !tasks.is_empty() {
    let mut heap_guard = heap.lock();
    for task in tasks {
        heap_guard.push(task);
    }
}
```

---

### 7. **Linear Search in Re-orderer**
**Location:** `reorder_loop` - Lines 683-694

**Problem:**
- Linear scan through 3 buffers to find earliest deadline
- Minimal impact (only 3 elements) but could be optimized

**Recommendation:**
- Use a small BinaryHeap or maintain sorted invariant
- Or just compare 3 values manually (current is fine)

---

### 8. **Dispatcher Yield on Empty**
**Location:** `dispatcher_loop` - Line 557

**Problem:**
- `thread::yield_now()` called every iteration when no work
- May cause excessive context switches

**Recommendation:**
- Use exponential backoff or small sleep
- Or use blocking receive with timeout

---

### 9. **Instant::now() Calls**
**Location:** Multiple locations

**Problem:**
- `Instant::now()` is called frequently:
  - Dispatcher: Every iteration (line 459)
  - Elasticity: Every 5µs check (line 609, 611, 626)
  - Re-ordering: Every packet (line 672)

**Impact:**
- System call overhead (typically 10-100ns, but adds up)

**Recommendation:**
- Cache `now` value within loops where possible
- Consider monotonic clock if available

---

## Low Priority Issues

### 10. **BinaryHeap Rebalancing**
- `BinaryHeap::push()` and `pop()` have O(log n) complexity
- Under high load, heap size grows, increasing rebalancing cost

**Mitigation:**
- Consider limiting heap size (drop oldest if full)
- Use more efficient heap structure if heap size > 1000

---

### 11. **Channel Operations**
- `try_recv()` and `try_send()` have overhead
- Consider batching if feasible

---

## Performance Metrics to Monitor

1. **Lock contention:**
   - Time spent waiting for `heap.lock()`
   - Time spent waiting for `distribution_stats.lock()`

2. **CPU utilization:**
   - Per-thread CPU usage
   - Context switch rate (should be minimal for real-time)

3. **Elasticity effectiveness:**
   - How often HIGH preempts during elasticity window
   - Average elasticity wait time

4. **Heap sizes:**
   - Average and maximum heap depth per EDF
   - Impact on `pop()` performance

---

## Quick Wins (Easy Fixes)

1. ✅ **Fix double lock** (Lines 481-497) - Easy, high impact
2. ✅ **Reduce cloning** (Lines 487-489) - Easy, medium impact  
3. ✅ **Batch heap pushes** (Lines 579-597) - Easy, medium impact
4. ⚠️ **Separate cores** - Requires testing, high impact
5. ✅ **Cache Instant::now()** - Easy, low impact

---

## Summary

**Most Critical:**
1. CPU contention (all EDFs on same core) - May cause deadline misses for HIGH
2. Lock contention on heaps - High frequency, adds latency
3. Elasticity loop overhead - Runs on every MEDIUM/LOW packet

**Recommended Priority:**
1. Fix double lock on distribution stats (5 min fix)
2. Reduce task cloning overhead (5 min fix)
3. Consider separating HIGH EDF to dedicated core (requires testing)
4. Batch heap operations (10 min fix)
5. Optimize elasticity loop (requires profiling)

