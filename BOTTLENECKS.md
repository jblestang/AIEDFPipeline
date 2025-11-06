# Performance Bottlenecks Analysis

## Current Bottlenecks (High Priority)

### 1. EDF Scheduler: Lock Held During Entire K-way Merge Loop ⚠️ HIGH IMPACT
**Location**: `src/edf_scheduler.rs:90-164`

**Issue**: The `tasks` mutex is held for the entire duration of the k-way merge loop, which includes:
- Multiple `try_recv()` operations from channels
- Deadline comparisons
- Heap insertions

**Impact**: 
- Blocks other threads from accessing the heap
- If the loop processes many packets, the lock is held for a long time
- Could cause contention with other operations

**Current Code**:
```rust
let mut tasks = self.tasks.lock();  // Lock acquired here
loop {
    // ... channel operations and comparisons ...
    tasks.push(task);  // Lock still held
}
drop(tasks);  // Lock released here
```

**Recommended Fix**:
- Collect packets from channels first (without lock)
- Compare and prepare all tasks
- Lock once to insert all tasks in batch
- Release lock before processing next packet

**Priority**: 🔴 HIGH

---

### 2. EDF Scheduler: Double Lock Acquisition ⚠️ MEDIUM IMPACT
**Location**: `src/edf_scheduler.rs:90-180`

**Issue**: The lock is acquired twice:
1. First lock (line 90): For inserting incoming packets
2. Second lock (line 167): For processing the next packet

**Impact**: 
- Unnecessary lock contention
- Could be combined into a single lock acquisition

**Current Code**:
```rust
let mut tasks = self.tasks.lock();  // First lock
// ... insert packets ...
drop(tasks);

let mut tasks = self.tasks.lock();  // Second lock
if let Some(task) = tasks.pop() {
    // ... process ...
}
```

**Recommended Fix**:
- Combine both operations into a single lock acquisition
- Process the next packet immediately after inserting new ones

**Priority**: 🟡 MEDIUM

---

### 3. Ingress DRR: Multiple Lock Acquisitions in Hot Loop ⚠️ HIGH IMPACT
**Location**: `src/ingress_drr.rs:103-199`

**Issue**: Multiple separate lock acquisitions per iteration:
- `active_flows.lock()` (line 103) - cloned
- `current_flow_index.lock()` (line 112)
- `socket_configs.lock()` (line 126) - inside loop
- `flow_states.lock()` (line 139) - inside loop
- `flow_states.lock()` again (line 156) - inside loop
- `current_flow_index.lock()` (line 199)

**Impact**:
- High lock contention
- Each lock acquisition has overhead
- Linear search in `socket_configs` (Vec) inside lock

**Current Code**:
```rust
let active_flows = self.active_flows.lock().clone();  // Lock 1
let mut current_index = *self.current_flow_index.lock();  // Lock 2
for _ in 0..max_iterations {
    let socket_configs = self.socket_configs.lock();  // Lock 3 (inside loop)
    let mut flow_states = self.flow_states.lock();  // Lock 4 (inside loop)
    // ...
    let mut flow_states = self.flow_states.lock();  // Lock 5 (inside loop)
}
*self.current_flow_index.lock() = current_index;  // Lock 6
```

**Recommended Fix**:
- Combine `socket_configs`, `flow_states`, `active_flows`, and `current_flow_index` into a single `IngressDRRState` struct
- Use a single mutex to protect all state
- Pre-compute socket lookups outside the loop
- Use `HashMap` for socket configs instead of `Vec` for O(1) lookup

**Priority**: 🔴 HIGH

---

### 4. Ingress DRR: Linear Search in Socket Configs ⚠️ MEDIUM IMPACT
**Location**: `src/ingress_drr.rs:127`

**Issue**: Using `Vec::find()` to locate socket config by `flow_id` inside a lock.

**Impact**:
- O(n) lookup time
- Lock held during search
- Inefficient for multiple flows

**Current Code**:
```rust
let socket_configs = self.socket_configs.lock();
let config = match socket_configs.iter().find(|c| c.flow_id == flow_id) {
    Some(c) => c,
    None => continue,
};
```

**Recommended Fix**:
- Change `socket_configs` from `Vec<SocketConfig>` to `HashMap<u64, SocketConfig>`
- O(1) lookup instead of O(n)

**Priority**: 🟡 MEDIUM

---

### 5. Egress DRR: Lock for Every Packet Lookup ⚠️ MEDIUM IMPACT
**Location**: `src/egress_drr.rs:72-75`

**Issue**: Every packet requires locking `output_sockets` to find the socket.

**Impact**:
- Lock contention on every packet
- Could be optimized with pre-computed socket map

**Current Code**:
```rust
let socket_opt = {
    let sockets = output_sockets.lock();  // Lock for every packet
    sockets.get(&packet.flow_id).map(|(s, a)| (s.clone(), *a))
};
```

**Recommended Fix**:
- Clone the socket map once at the start of processing
- Use the cloned map for lookups (no lock needed)
- Periodically refresh the clone if sockets are added/removed

**Priority**: 🟡 MEDIUM

---

### 6. Ingress DRR: Lock Acquired Multiple Times for Same Flow State ⚠️ LOW IMPACT
**Location**: `src/ingress_drr.rs:139-160`

**Issue**: `flow_states` lock is acquired twice for the same flow in one iteration:
1. First lock (line 139): To update deficit
2. Second lock (line 156): To decrement deficit after packet read

**Impact**:
- Unnecessary lock overhead
- Could be combined into a single lock acquisition

**Current Code**:
```rust
let mut flow_states = self.flow_states.lock();  // Lock 1
flow.deficit += flow.quantum;
drop(flow_states);

// ... socket read ...

let mut flow_states = self.flow_states.lock();  // Lock 2
flow.deficit -= 1;
```

**Recommended Fix**:
- Keep the lock until after the packet is read
- Update deficit in a single lock acquisition

**Priority**: 🟢 LOW

---

## Potential Optimizations (Lower Priority)

### 7. EDF Scheduler: Channel Operations Inside Lock
**Location**: `src/edf_scheduler.rs:96-114`

**Issue**: `try_recv()` operations are performed while the heap lock is held.

**Impact**: 
- Lock held during potentially blocking operations
- Could delay other threads unnecessarily

**Recommended Fix**:
- Collect all packets from channels first (without lock)
- Then lock once to insert all collected packets

**Priority**: 🟡 MEDIUM

---

### 8. Ingress DRR: Cloning Active Flows
**Location**: `src/ingress_drr.rs:103`

**Issue**: Cloning the entire `active_flows` vector on every iteration.

**Impact**:
- Memory allocation overhead
- Could be avoided with better locking strategy

**Recommended Fix**:
- Use a read-write lock or atomic reference counting
- Or combine with other state into single lock

**Priority**: 🟢 LOW

---

### 9. Egress DRR: Async Socket Send
**Location**: `src/egress_drr.rs:77`

**Issue**: Using `await` for socket send could introduce latency.

**Impact**:
- Async overhead
- Could use blocking send in a dedicated thread

**Recommended Fix**:
- Consider using blocking `send_to` in a dedicated thread
- Or batch sends to reduce async overhead

**Priority**: 🟢 LOW

---

## Summary

### High Priority Fixes:
1. ✅ **EDF Scheduler: Lock held during entire loop** - Collect packets first, then lock once
2. ✅ **Ingress DRR: Multiple lock acquisitions** - Combine into single state struct

### Medium Priority Fixes:
3. ✅ **EDF Scheduler: Double lock acquisition** - Combine operations
4. ✅ **Ingress DRR: Linear search in socket configs** - Use HashMap
5. ✅ **Egress DRR: Lock for every packet** - Clone socket map
6. ✅ **EDF Scheduler: Channel ops inside lock** - Move outside lock

### Low Priority Fixes:
7. ✅ **Ingress DRR: Multiple locks for same flow** - Combine lock acquisitions
8. ✅ **Ingress DRR: Cloning active flows** - Better locking strategy
9. ✅ **Egress DRR: Async socket send** - Consider blocking send

---

## Performance Impact Summary

| Bottleneck | Impact | Fix Complexity | Estimated Improvement |
|------------|--------|----------------|----------------------|
| EDF Lock Duration | 🔴 High | Medium | 20-30% latency reduction |
| Ingress DRR Multiple Locks | 🔴 High | Medium | 15-25% throughput increase |
| EDF Double Lock | 🟡 Medium | Low | 5-10% latency reduction |
| Ingress DRR Linear Search | 🟡 Medium | Low | 10-15% throughput increase |
| Egress DRR Socket Lookup | 🟡 Medium | Low | 5-10% latency reduction |
| Ingress DRR Flow State Locks | 🟢 Low | Low | 2-5% improvement |

---

## Recommended Implementation Order

1. **First**: Fix EDF scheduler lock duration (collect packets first, then lock)
2. **Second**: Combine Ingress DRR locks into single state struct
3. **Third**: Change socket configs to HashMap
4. **Fourth**: Optimize Egress DRR socket lookup
5. **Fifth**: Combine EDF double lock acquisition
