# Scheduler Benchmark Report
_Generated at UNIX timestamp 1762889082_

| Scheduler | Throughput (pkt/s) | Flow1 P95 (ms) | Flow1 First (ms) | Loss % |
|-----------|-------------------|----------------|------------------|--------|
| single | 721.73 | 116278.052 | 1.812 | 5.76 |
| multi-worker | 422.80 | 45403.407 | 0.000 | 52.81 |
| gedf | 728.82 | 115007.386 | 1.514 | 1.45 |
| gedf-vd | 807.27 | 114867.581 | 0.000 | -15.68 |

- **Best Flow1 P95 latency:** multi-worker (45403.407 ms)
- **Highest throughput:** gedf-vd (807.27 packets/s)
