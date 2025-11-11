# Pipeline Architecture

## End-to-End Flow
```
┌─────────────────────────────────────────────────────────────────────┐
│                       UDP INPUT SOCKETS (per priority)               │
│  127.0.0.1:8080 → High (1 ms)                                        │
│  127.0.0.1:8081 → Medium (10 ms)                                     │
│  127.0.0.1:8082 → Low (100 ms)                                       │
│  Best Effort → internal metrics sources                              │
└──────────────┬───────────────────────────────────────────────────────┘
               │
               ▼
┌─────────────────────────────────────────────────────────────────────┐
│                     Ingress DRR Scheduler (core 0)                  │
│  - Non-blocking UDP recv                                            │
│  - Packet-count DRR with configurable quantum per Priority          │
│  - Drops recorded when ingress_to_edf channel is full               │
└───────┬──────────────┬──────────────┬──────────────┬──────────────┘
        │              │              │              │
        ▼              ▼              ▼              ▼
PriorityTable<Sender<Packet>> bounded channels (default 16 each)
        │              │              │              │
        ▼              ▼              ▼              ▼
┌─────────────────────────────────────────────────────────────────────┐
│                        EDF Processor (core 1)                       │
│  - One pending slot per priority                                    │
│  - Selects earliest deadline among head packets                      │
│  - Simulates work (0.2–0.4 ms) via busy spin                         │
│  - Drops recorded when edf_to_egress channel is full                 │
└───────┬──────────────┬──────────────┬──────────────┬──────────────┘
        │              │              │              │
        ▼              ▼              ▼              ▼
PriorityTable<Sender<Packet>> bounded channels (default 16 each)
        │              │              │              │
        ▼              ▼              ▼              ▼
┌─────────────────────────────────────────────────────────────────────┐
│                      Egress DRR Scheduler (core 2)                  │
│  - Pulls in priority order (High → Medium → Low → BestEffort)       │
│  - Records latency/deadline metrics via lock-free channel           │
│  - Emits over Tokio `UdpSocket::send_to`                             │
└──────────────┬───────────────────────────────────────────────────────┘
               │
               ▼
┌─────────────────────────────────────────────────────────────────────┐
│                       UDP OUTPUT SOCKETS (per priority)              │
│  127.0.0.1:9080 ← High                                              │
│  127.0.0.1:9081 ← Medium                                            │
│  127.0.0.1:9082 ← Low                                               │
│  Best Effort ← optional (metrics stay local by default)              │
└─────────────────────────────────────────────────────────────────────┘
```

## Threading Model
```
┌──────────────────────────────────────────────────────────────┐
│ Ingress-DRR thread (core 0, priority 2)                       │
│  - Tokio current-thread runtime                               │
│  - Reads UDP, enqueues into PriorityTable channels            │
│  - Maintains DRR state behind a single mutex                  │
└──────────────────────────────────────────────────────────────┘
┌──────────────────────────────────────────────────────────────┐
│ EDF-Processor thread (core 1, priority 2)                     │
│  - Tight loop over EDF pending table                          │
│  - Busy-spins to emulate deterministic processing time        │
│  - Uses `PriorityTable<Option<EDFTask>>` to avoid heap churn  │
└──────────────────────────────────────────────────────────────┘
┌──────────────────────────────────────────────────────────────┐
│ Egress-DRR thread (core 2, priority 1)                        │
│  - Tokio current-thread runtime                               │
│  - Pulls by priority, records metrics, sends UDP              │
└──────────────────────────────────────────────────────────────┘
┌──────────────────────────────────────────────────────────────┐
│ Statistics thread (core 2, priority 1 – Best Effort lane)     │
│  - Drains metrics events from lock-free channel               │
│  - Computes percentiles, queue occupancy, drop counts         │
│  - Broadcasts JSON snapshots via TCP (default 127.0.0.1:9999, overridable with `--metrics-bind`) │
└──────────────────────────────────────────────────────────────┘
┌──────────────────────────────────────────────────────────────┐
│ GUI / external clients                                        │
│  - Connect to metrics TCP server                              │
│  - Visualise latency, drops, queue levels                     │
└──────────────────────────────────────────────────────────────┘
```

## Configuration Overview
`PipelineConfig` (see `src/pipeline.rs`) groups all tunables:
- `CoreAssignment`: pins ingress, EDF, and egress/metrics threads to specific logical cores.
- `QueueConfig`: per-priority channel capacities (`ingress_to_edf`, `edf_to_egress`).
- `IngressSchedulerConfig`: per-priority quantum (packet counts).
- `EdfSchedulerConfig`: maximum pending depth (currently informational).

Because these structs embed `PriorityTable`, adding a new priority only requires extending `Priority::ALL` and supplying defaults.

## Metrics Flow
1. Egress records latency/deadline results via `metrics_collector.record_packet(priority, ...)`.
2. Events go into a lock-free `crossbeam_channel` so the hot path avoids mutexes.
3. The statistics thread (Best Effort priority) aggregates metrics, merges ingress/EDF drop counters, and streams snapshots.
4. The GUI plots latency percentiles, queue usage, and drop series with a legend identifying ingress versus EDF drops.

## Adaptive Load Balancing
The multi-worker EDF scheduler uses an adaptive controller that keeps latency budgets on track:

- **Processing-time EMA** – every worker feeds its observed processing duration back into a per-priority exponential moving average (default decay `7/8 ↔ 1/8`).
- **Capacity estimation** – every 100 ms we estimate the safe backlog per priority via `target ≈ 0.9 × budget / avg_processing`, clamped between 1 and 512 packets.
- **Quota distribution** – High priority is anchored on Worker 0, with small spillover allowances on workers 1 and 2. Medium and Low are split across their eligible workers with hard caps (e.g. Medium ≤160 per worker, Low ≤192).
- **Guard tuning** – Medium/Low guard windows are capped at roughly 80 µs. High always preempts immediately, whereas Medium only guards while the local High backlog is empty.
- **Dynamic atomics** – all limits/thresholds live in atomics so workers read them lock-free each loop. A dedicated `EDF-AutoBalance` thread rewrites the atomics every 100 ms based on the latest EMA and queue depth.

This feedback loop keeps High P50 close to the processing floor while capping P99 under the configured 1 ms budget even under bursty load.

## Scheduler Options
- **Single** – classic single-thread EDF (default).
- **MultiWorker** – adaptive per-priority workers with local quotas.
- **Global (G-EDF)** – shared run queue serviced by multiple workers; the earliest deadline across all priorities always wins.
- **Global VD (G-EDF-VD)** – global EDF with per-priority virtual deadlines (High keeps real deadlines, lower priorities are tightened) to reduce deadline misses for critical traffic.

Select the strategy at runtime via `--scheduler single|multi|gedf|gedf-vd`.

## License & Compliance
- All networking remains on localhost UDP.
- GUI dependencies (`egui`, `eframe`, `egui_plot`) are included with minimal feature sets and MIT/Apache compatible licenses.
- GPL-licensed crates are denied through `deny.toml`.

This architecture keeps real-time workloads isolated on dedicated cores while leaving the Best Effort lane to handle telemetry and control plane work.

