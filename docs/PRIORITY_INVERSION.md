# Priority Inversion Analysis

## Current Thread Layout
| Thread | Core (default) | Priority Hint | Responsibility |
|--------|----------------|---------------|----------------|
| Ingress-DRR (Tokio runtime) | Core 0 | `set_thread_priority(2)` | Poll non-blocking UDP sockets, run ingress DRR, enqueue to EDF. |
| EDF-Processor (std::thread) | Core 1 | `set_thread_priority(2)` | Drain EDF pending table, simulate work, forward to egress. |
| Egress-DRR (Tokio runtime) | Core 2 | `set_thread_priority(1)` | Pull from EDF outputs, record metrics, send UDP replies. |
| Statistics-Thread | Core 2 (Best Effort) | `set_thread_priority(1)` | Drain metrics events, compute percentiles, broadcast snapshots. |
| Metrics TCP Tasks (Tokio) | Shared | default | Accept GUI clients, forward JSON snapshots. |

The default `PipelineConfig` pins the metrics/statistics thread to the same core as egress, designated as the Best Effort lane. Metrics recording uses a lock-free channel so the high-priority EDF/DRR threads avoid blocking on shared locks.

## Shared Resources
- `IngressDRRScheduler::state (Mutex<IngressDRRState>)`: Accessed only by the ingress thread. No cross-thread contention → no inversion risk.
- `EDFScheduler::pending (Mutex<PriorityTable<Option<EDFTask>>>)`: Accessed solely by the EDF thread. No inversion risk.
- `EgressDRRScheduler::output_sockets (Mutex<HashMap<...>>)` : Only mutated during start-up before the egress loop clones the map. No runtime contention.
- `MetricsCollector::metrics (Mutex<HashMap<Priority, Metrics>>)` : Guarded by the statistics thread; writers on the hot path push events via `events_tx` and never touch the mutex directly.

## Residual Risks
1. **Metrics Snapshot Lag**
   - If the statistics thread takes longer than the publish interval (e.g., due to expensive percentile calculations) it still runs on the best-effort core. High-priority threads are unaffected, but stale metrics could hide emerging issues.
   - *Mitigation*: percentiles are cached and recomputed only when the packet count changes. Keep monitoring CPU usage on the best-effort core during heavy traffic.

2. **OS-Level Socket Locks**
   - UDP send/receive ultimately touches kernel resources. If the OS pauses the egress thread (priority 1) while the EDF thread waits for channel capacity to free, deadlines can still be impacted. This is inherent to user-space scheduling; larger socket buffers and balanced quantums alleviate the problem.

3. **Future Priority Additions**
   - When adding new classes via `Priority::ALL`, verify that the best-effort lane still owns telemetry work. If a higher-priority stream starts publishing metrics, ensure it goes through the lock-free event channel.

## Recommendations
- Keep the statistics thread on the best-effort core (`cores.egress` by default) so metrics never contend with the high-priority data path.
- If additional background services are introduced, co-locate them with Best Effort and confirm they avoid shared locks with the high-priority threads.
- Monitor queue occupancy and drop counters to detect indirect priority inversions caused by backlog rather than locking.

## Conclusion
The refactored pipeline routes all mutex-protected state to the threads that own it, while real-time stages interact through bounded lock-free channels. With metrics ingestion completely decoupled, no classical priority inversion remains on the hot path. Residual concerns are tied to OS scheduling and kernel-level buffering rather than user-space locks. Continuous monitoring of drops and latency percentiles remains essential to catch regressions early.

