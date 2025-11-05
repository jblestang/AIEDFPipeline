#!/bin/bash

# Example script to send packets to different input sockets
# Each socket has a different latency budget

echo "Sending test packets to pipeline..."

# Flow 1: 1ms latency budget (port 8080)
echo "Sending 'Hello' to Flow 1 (1ms latency) on port 8080"
echo -n "Hello" | nc -u 127.0.0.1 8080

sleep 0.1

# Flow 2: 50ms latency budget (port 8081)
echo "Sending 'World' to Flow 2 (50ms latency) on port 8081"
echo -n "World" | nc -u 127.0.0.1 8081

sleep 0.1

# Flow 3: 100ms latency budget (port 8082)
echo "Sending 'Test' to Flow 3 (100ms latency) on port 8082"
echo -n "Test" | nc -u 127.0.0.1 8082

echo "Packets sent!"

