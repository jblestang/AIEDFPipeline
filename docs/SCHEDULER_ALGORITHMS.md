# Scheduler Algorithms

This document summarizes the scheduling strategies implemented in the project. Each section outlines the algorithm’s responsibilities, execution flow, data structures, and unique behaviors.

---

## Ingress Deficit Round Robin (DRR)

### Role
- Front-line UDP ingress layer.
- Performs packet-count based Deficit Round Robin (DRR) before enqueuing packets into per-priority bounded channels.
- Ensures fairness and avoids starvation prior to EDF processing.

### Threading
- Runs inside its own Tokio current-thread runtime, typically on a dedicated core.

### Core Data Structures
- `PriorityTable<Sender<Packet>>`
- Mutex-protected `IngressDRRState`, maintaining socket configurations, flow states, and active flow pointers.
- Each `FlowState` contains DRR quantum, atomic deficit counter, and latency budget.
- UDP sockets are wrapped in `Arc`, facilitating concurrency.

### Main Loop (`IngressDRRScheduler::process_sockets`)
1. Repeatedly snapshots DRR state (active flows, sockets, flow state) to minimize mutex holding.
2. For each active priority in round-robin order:
   - Adds quantum allowances to the atomic deficit counter.
   - Attempts UDP receive with buffer sized via `FIONREAD`.
   - On successful receive, translates buffer to a `Packet` with correct timestamp and latency budget; enqueues via non-blocking channel send.
3. On full channel, increments per-priority drop counter and proceeds to next flow.
4. MBSU adjustments use atomic operations for thread-safety without additional locks.

### Drop Handling
- Drops occur only when the crossbeam channel for a priority is full.
- The drop is recorded through `drop_counters[priority]`.

### Unique Notes
- Uses zero-copy buffer pool ensuring minimal allocations.
- Maintains round-robin fairness via atomic deficit counters.

---

## EDF Scheduler

### Role
- Single-threaded Earliest Deadline First (EDF) scheduler.
- Selects the head-of-line packet across priorities based on deadline, processing one packet at a time.

### Threading
- In single-threaded mode, runs in its own OS thread pinned to a designated core.

### Core Data Structures
- `PriorityTable<Option<EDFTask>>` for head-of-line tasks per priority.
- `PriorityTable<Arc<Receiver<Packet>>>` input queues holding crossbeam channels.
- `PriorityTable<Sender<Packet>>` output queues.
- `PriorityTable<Arc<AtomicU64>>` drop counters for a priority; maintains drop stats when egress queues are full.

### Processing Loop (`EDFScheduler::process_next`)
1. Acquire pending lock.
2. For each priority, if pending slot empty, pulls from input queue using `try_recv`.
3. Finds earliest deadline across pending tasks.
4. Releases lock, processes selected packet with simulated busy-wait `processing_duration`.
5. Posts processed packet to output queue; increments drop counters if output queue is full.

### Drop Handling
- Drop recorded when `output_queues[priority].try_send(packet)` fails due to full channel.

### Unique Notes
- No heap or overflow buffer; simplest EDF variant.
- Maintains monotonic packet IDs for egress verification.

---

## Multi-Worker EDF Scheduler

### Role
- Parallel EDF variant using multiple workers and adaptive load balancing.
- Each worker draws from dedicated priority subsets (configured in `WORKER_ASSIGNMENTS`) and maintains per-worker min-deadline heap.

### Threading
- One dispatcher thread per worker (Rust thread for each worker).
- Optional auto-balancing thread adjusting quotas.
- `AdaptiveController` uses atomic structures to manage backlog limits and guard thresholds.

### Core Data Structures
- `PriorityTable<Arc<Receiver<Packet>>>` input queues consumed by workers.
- `CompletionRouter` ensures per-priority in-order delivery using sequence tracking.
- `AdaptiveController` with arrays of `AtomicUsize` for total limits and per-priority limits per worker.
- `WORKER_ASSIGNMENTS`: defines priorities each worker handles (Worker 0: High, Worker 1: High + Medium, Worker 2: High + Medium + Low + Best Effort).
- `PriorityTable<Arc<AtomicUsize>>` for worker backlog metrics and GUI reporting.

### Processing (`run_worker`)
1. For each assigned priority, attempts to pull packets respecting per-priority and total quotas.
   - `push_with_capacity` imposes quotas; avoids overflow.
2. Maintains per-worker min-deadline heap (`BinaryHeap` reversed for earliest deadline).
3. When selecting next task:
   - Provides immediate preemption opportunity: newly arriving High priority can replace scheduled task.
   - Implements guard windows based on deadline aging before processing lower priorities, ensuring high priorities can interrupt early.
4. Simulates processing via busy-wait using `processing_duration`.
5. Completion done via `CompletionRouter::complete`, ensuring final send to output queue while maintaining sequence order.

### Auto-Balancing (`AdaptiveController::autobalance_loop`)
1. Runs periodically (100 ms).
2. Calculates safe backlog based on EMA of processing times and expected latency budgets.
3. Adjusts per-worker total limits and per-priority quotas.
4. Tuning parameters include:
   - High priority quotas distribution across workers.
   - Guard thresholds for Medium/Low stored as microseconds.
   - Guard slice (`guard_slice_us`) controlling mini-polls for High priority bursts.
5. Collects worker metrics for GUI display from `worker_priority_counters`.

### Drop Handling
- Drops occur when worker output queues refuse `try_send` (via `CompletionRouter::complete`).
- Drop counts recorded in `drop_counters`.
- Adaptive quotas prevent unbounded queues to mitigate drop cascade.

### Unique Notes
- `CompletionRouter` ensures per-priority FIFO order using `SequenceTracker` with `next_emit`.
- Guard window ensures fairness when higher priority tasks likely to appear soon.
- Worker backlogs are tracked for GUI metrics display.

---

## Global EDF (GEDF)

### Role
- Global Earliest Deadline First where multiple worker threads share a single run queue (`SharedQueue`).
- Dispatcher thread merges all priority queues, workers pop from shared heap.

### Threading
- One dispatcher thread pinned to designated core.
- Multiple worker threads (configurable) pinned to chosen cores.

### Core Data Structures
- `SharedQueue`: `Mutex<BinaryHeap<QueuedTask>>` with `Condvar`.
- `PriorityTable<Arc<Receiver<Packet>>>` for inputs.
- `PriorityTable<Sender<Packet>>` for outputs.
- `PriorityTable<Arc<AtomicU64>>` for drop counters.

### Processing
1. Dispatcher:
   - Continuously tries non-blocking receive for each priority.
   - Pushes tasks with computed real deadlines into `SharedQueue`.
   - Yields when no packet dispatched.
2. Workers:
   - Wait on shared queue’s `Condvar`.
   - On pop, simulate processing time, try send to output queue.
   - Record drops via `drop_counters[priority]`.
3. When shutting down, `shared_queue.wake_all()` ensures workers exit promptly.

### Drop Handling
- Same as other algorithms: drop counters incremented when output channel full.

### Unique Notes
- Simplest multi-threaded EDF variant; no per-worker quotas or guard windows.
- Worker list uses `worker_cores` fallback to dispatcher core if none provided.

---

## Global EDF with Virtual Deadlines (GEDF-VD)

### Role
- Extends GEDF by scaling latency budgets to produce virtual deadlines for mid/low priorities, reducing starvation.
- Lower priority tasks get an earlier virtual deadline (latency budget × scaling factor).

### Threading
- Similar to `GEDFScheduler`: dispatcher plus worker threads.
- Uses same `SharedQueue` structure but tasks include both actual and virtual deadlines.

### Core Data Structures
- Inherits from GEDF with `PriorityTable<f64>` scaling factors.

### Processing
1. Dispatcher:
   - On packet arrival, computes scaled latency via `latency_budget.mul_f64(scaling[priority].clamp(0.1, 1.0))`.
   - Pushes `QueuedTask { virtual_deadline, priority, packet }`.
2. Workers identical to GEDF: pop earliest virtual deadline, simulate processing, try send, record drops.

### Drop Handling
- Same as GEDF.

### Unique Notes
- Virtual deadlines ensure mediums/lows get more immediate slots (prevent extended starvation under heavy high priority).
- Scaling factors currently: `High=1.0`, `Medium=0.75`, `Low=0.6`, `BestEffort=0.5`.

---

## Egress DRR

### Role
- Final stage reading EDF output queues, writing to UDP sockets, and recording metrics/drops.

### Threading
- Runs on its own Tokio current-thread runtime, often on same core as ingress in a combined IO thread.

### Core Data Structures
- `PriorityTable<Receiver<Packet>>` for outputs from EDF.
- `PriorityTable<Arc<AtomicU64>>` storing last packet IDs per priority for monotonicity check.
- `Arc<Mutex<HashMap<Priority, (Arc<UdpSocket>, SocketAddr)>>>` mapping to actual output sockets.

### Processing Flow
1. `process_queues` spawns blocking task:
   - Loops while pipeline running flag true.
   - For each priority in fixed order:
     - `try_recv` from EDF channel.
     - Verifies packet IDs for strict ascending order.
     - Records latency and deadline miss via `MetricsCollector::record_packet`.
     - Attempts non-blocking UDP send with busy wait for `WouldBlock`.
2. When no packet processed in iteration, yields thread to not starve others.

### Drop Handling
- When UDP send fails (due to local queue), multiple retries attempted until success or non-blocking error permanently hits.
- If EDF queue `try_recv` returns `Disconnected`, thread exits.

### Unique Notes
- Maintains per-priority last packet ID to detect out-of-order emission (assertion feature).
- Leverages metrics pipeline to send per-packet latency data.
*** End Patch

