# Packet Drop Analysis

## Overview
The current pipeline keeps the hot path short: packets travel UDP → ingress DRR → EDF → egress DRR → UDP. Bounded crossbeam channels protect each transition. Drops can still occur whenever a bounded channel is full or when the external UDP stack refuses traffic. This document lists every drop point, the default capacities, and how we expose the counters through metrics.

## Pipeline Flow (Default Configuration)
```
UDP Socket (non-blocking)
  ↓
IngressDRRScheduler::process_sockets()
  ↓ [DROP POINT 1: ingress_to_edf channel full]
PriorityTable<Sender<Packet>> (bounded 16 per priority)
  ↓
EDFScheduler::process_next()
  ↓ [DROP POINT 2: edf_to_egress channel full]
PriorityTable<Sender<Packet>> (bounded 16 per priority)
  ↓
EgressDRRScheduler::process_queues()
  ↓
UDP Socket (Tokio send_to)
```

The Best Effort class shares the same channel infrastructure but is currently used for metrics and other utility traffic.

## Drop Points

### 1. Ingress → EDF Channel (per priority)
- **Location**: `IngressDRRScheduler::process_sockets` before calling `try_send` on `priority_queues[priority]`.
- **Capacity**: `queues.ingress_to_edf[priority]` (defaults to 16 for all priorities).
- **Behaviour**: When `try_send` returns `Err(Full)`, the packet is dropped and the per-priority atomic counter is incremented (`drop_counters[priority]`).
- **Metrics exposure**: Reported as `ingress_drops` in the metrics snapshot and plotted in the GUI legend (“Ingress Drops”).
- **Tuning**: Increase the channel size for the affected priority or adjust its DRR quantum.

### 2. EDF → Egress Channel (per priority)
- **Location**: `EDFScheduler::process_next` when forwarding to `output_queues[priority]`.
- **Capacity**: `queues.edf_to_egress[priority]` (defaults to 16 for all priorities).
- **Behaviour**: The scheduler only holds one pending packet per priority. If the downstream bounded channel is full, the packet is dropped and `output_drop_counters[priority]` is incremented.
- **Metrics exposure**: Reported as `edf_output_drops` (“EDF Output Drops” in the GUI legend).
- **Tuning**: Increase the channel size, reduce simulated processing time (smaller payloads), or balance DRR quantums so the egress side keeps up.

### 3. UDP Emit (socket backpressure)
- **Location**: `EgressDRRScheduler::process_queues`, inside `socket.send_to`.
- **Behaviour**: Tokio’s `send_to` returns an error if the OS transmit buffer is full. The current implementation ignores the error, effectively dropping the packet. This path is outside the scheduler’s bounded channels so we rely on system-level buffering.
- **Metrics exposure**: No explicit counter today. If this becomes an issue, wrap `send_to` and surface a best-effort drop metric.
- **Tuning**: Increase OS socket buffers or dedicate separate sockets for congested priorities.

## Non-Drop Behaviours Worth Noting
- **EDF Pending Table**: The EDF scheduler no longer backs packets with a shared heap. It buffers at most one head-of-line packet per priority and never drops because of `max_heap_size`. The configuration parameter remains for compatibility but does not cause drops.
- **Best Effort class**: Metrics and utility messages use Best Effort. Unless an output socket is provisioned for that class, packets simply terminate inside the runtime.

## Metrics Summary
| Counter | Source | GUI Label |
|---------|--------|-----------|
| `ingress_drops` | Ingress DRR `drop_counters` | Ingress Drops |
| `edf_output_drops` | EDF `output_drop_counters` | EDF Output Drops |
| `edf_heap_drops` | Fixed at 0 (heap removed) | EDF Heap Drops |

All counters are published by `MetricsCollector::send_current_metrics` and aggregated per priority. The GUI legend differentiates ingress versus EDF drop sources so it is easy to see where congestion occurs.

## Recommended Monitoring
1. Watch the drop plots in the GUI (or parse the TCP metrics feed) while running the stress example.
2. Track queue occupancy percentages; sustained >80% indicates upcoming drops.
3. Capture OS-level UDP statistics (`netstat`, `ss`, or `nettop`) to detect kernel buffer exhaustion.

## Next Steps for Improved Visibility
- Hook `send_to` errors into a Best Effort metrics event to account for transport-level drops.
- Add exponential moving averages of drop rates for alerting.
- Allow per-priority socket buffer tuning via `PipelineConfig` for tighter control in production deployments.

