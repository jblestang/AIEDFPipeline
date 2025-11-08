# Using the AIEDF Pipeline Examples

This directory contains helper snippets for sending and receiving traffic through the pipeline while observing latency guarantees and drop counters.

## Quick Start
1. **Launch the pipeline**
   ```bash
   cargo run --release
   ```
   The binary binds the default sockets:
   - Inputs: `127.0.0.1:8080` (High, 1 ms), `8081` (Medium, 10 ms), `8082` (Low, 100 ms)
   - Outputs: `127.0.0.1:9080/9081/9082` for the same priorities
   - Metrics server: `127.0.0.1:9999`

2. **Optionally launch the GUI** (after the pipeline is running):
   ```bash
   cargo run --release --bin gui
   ```
   The dashboard visualises latency percentiles, queue occupancy, and ingress/EDF drop legends.

## Sending Packets
Use any UDP client. The socket you choose sets the priority class automatically.
```bash
# High priority (1 ms)
echo -n "Hello" | nc -u 127.0.0.1 8080

# Medium priority (10 ms)
echo -n "World" | nc -u 127.0.0.1 8081

# Low priority (100 ms)
echo -n "Test" | nc -u 127.0.0.1 8082
```

## Receiving Packets
Open listeners on the output ports to verify delivery:
```bash
nc -u -l 9080   # High
nc -u -l 9081   # Medium
nc -u -l 9082   # Low
```

## Stress Test
Run the built-in stress harness to generate random payload sizes (0–1400 bytes) and mixed priorities:
```bash
cargo run --example stress_test
```
The test shows how ingress/EDF drop counters behave under sustained load.

## Socket & Priority Reference
| Priority | Input Port | Output Port | Latency Budget |
|----------|------------|-------------|----------------|
| High     | 8080       | 9080        | 1 ms           |
| Medium   | 8081       | 9081        | 10 ms          |
| Low      | 8082       | 9082        | 100 ms         |
| BestEffort | (internal) | optional   | none (metrics) |

## Customising the Pipeline
All capacities and DRR quanta live in `PipelineConfig` (`src/pipeline.rs`). Example override:
```rust
let mut config = PipelineConfig::default();
config.queues.ingress_to_edf[Priority::High] = 32;
config.ingress.quantums[Priority::Low] = 2;
let pipeline = Pipeline::new(config).await?;
```
Because the API uses `PriorityTable`, you can extend `Priority::ALL` with a new class without changing function signatures.

## Troubleshooting
1. **No packets received**: Ensure the pipeline is running and you are targeting the correct port.
2. **High drop counts**: Increase the relevant queue capacity or adjust DRR quanta in `PipelineConfig`.
3. **GUI cannot connect**: Confirm the metrics server is listening on `127.0.0.1:9999` and launch the GUI after the pipeline.
4. **Permission errors during tests**: Integration tests that bind UDP sockets are ignored by default; run them explicitly only when your environment allows it.
