#!/bin/bash

# Example script to receive packets from output sockets
# Each socket corresponds to a different flow

echo "Listening for packets from pipeline..."
echo "Press Ctrl+C to stop"
echo ""

# Listen on all three output sockets in separate terminals
# For a single terminal, you can use one at a time

echo "Flow 1 output (1ms latency) on port 9080:"
echo "Run: nc -u -l 9080"
echo ""
echo "Flow 2 output (50ms latency) on port 9081:"
echo "Run: nc -u -l 9081"
echo ""
echo "Flow 3 output (100ms latency) on port 9082:"
echo "Run: nc -u -l 9082"
echo ""

# Listen on Flow 1 by default
nc -u -l 9080

