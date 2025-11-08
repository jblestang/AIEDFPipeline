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
│  - Broadcasts JSON snapshots via TCP 127.0.0.1:9999           │
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

## License & Compliance
- All networking remains on localhost UDP.
- GUI dependencies (`egui`, `eframe`, `egui_plot`) are included with minimal feature sets and MIT/Apache compatible licenses.
- GPL-licensed crates are denied through `deny.toml`.

This architecture keeps real-time workloads isolated on dedicated cores while leaving the Best Effort lane to handle telemetry and control plane work.

