# Adaptive Load Balancing in the Multi-Worker EDF Scheduler

The multi-worker EDF pipeline ships with a feedback controller that keeps latency budgets on target
without hard-coding per-priority queue sizes. This document describes the algorithm and the tuning
knobs exposed through the implementation.

## Inputs

- **Latency budgets** – sourced from the pipeline configuration (the tighter of ingress/egress
  socket budgets).
- **Observed processing time** – each worker records the actual duration spent on a packet and feeds
  it to the controller as soon as work completes.
- **Live queue depth** – per-worker/per-priority counts are tracked via atomics, giving the
  controller visibility into how many packets are queued when the rebalance runs.

## Exponential Moving Average (EMA)

The controller tracks a per-priority EMA of processing time:

```
ema_ns := max(1, (ema_ns * 7 + observed_ns) / 8)
```

This keeps the estimate stable in steady state while reacting quickly to sustained shifts (e.g.
higher payload sizes or additional CPU load).

## Rebalance Loop (every 100 ms)

1. **Capacity estimation** – for each priority:
   ```
   target_capacity = clamp(round(0.9 × budget_ns / ema_ns), 1, 512)
   ```
   The 0.9 factor intentionally over-allocates High to keep tail latency low.

2. **Worker distribution** – the target capacity is apportioned across eligible workers:
   - Worker 0 receives the bulk of High traffic; workers 1 and 2 get small spillover quotas.
   - Medium traffic is split across workers 1 and 2 with hard caps (160 each).
   - Low traffic is confined to worker 2 (cap 192).
   - Best Effort receives a fixed 128-packet cushion for metrics/logging workloads.

3. **Guard tuning** – Medium and Low guard windows are recomputed from the same EMA. High traffic
   always preempts immediately; Medium guards are skipped altogether when the local High backlog is
   non-zero.

4. **Atomic update** – total/priority quotas and guard parameters are written to atomics so worker
   threads read them without locking.

## Admission Checks

Workers consult the current quota before admitting new packets. Beyond the quota check, each worker
predicts the completion time of the candidate packet using the EMA plus the queued work; if the
projected finish would violate the latency budget the packet is dropped immediately instead of
sitting in the heap. Since the controller updates frequently, the system converges quickly after
load swings.

## Extensibility

- To experiment with different safety factors, tweak the multiplier (`0.9`) in
  `AdaptiveController::autobalance_loop`.
- Additional priorities only require extending `Priority::ALL`; the controller uses `PriorityTable`
  so arrays grow automatically.
- More sophisticated admission tests (e.g. checking remaining budget vs. backlog) can be added in
  `push_with_capacity` using the same EMA machinery.

This adaptive layer keeps the EDF workers near their latency budgets while avoiding hand-tuned,
hardware-specific queue sizes.

