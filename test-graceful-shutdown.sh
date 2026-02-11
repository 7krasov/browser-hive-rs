#!/bin/bash
# Test script for graceful shutdown

set -e

echo "=== Browser Hive Graceful Shutdown Test ==="
echo ""

# Colors for output
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m' # No Color

echo -e "${YELLOW}Step 1: Starting services with docker-compose${NC}"
docker-compose up -d

echo ""
echo -e "${YELLOW}Step 2: Waiting for services to be ready (10s)${NC}"
sleep 10

echo ""
echo -e "${YELLOW}Step 3: Checking worker health${NC}"
curl -s http://localhost:9090/metrics | grep browser_hive_worker || echo "Worker metrics not available yet"

echo ""
echo -e "${YELLOW}Step 4: Sending a long-running request (60s timeout) in background${NC}"
echo "This will simulate a request in progress during shutdown"

# Start request in background and capture PID
(
  grpcurl -plaintext -d '{
    "scope_name": "local_dev",
    "url": "https://example.com",
    "timeout_seconds": 60,
    "wait_timeout_ms": 5000
  }' localhost:50051 scraper.coordinator.ScraperCoordinator/ScrapePage > /tmp/test_response.json 2>&1

  if [ $? -eq 0 ]; then
    echo -e "${GREEN}✓ Request completed successfully${NC}"
    cat /tmp/test_response.json | jq -r '.errorMessage // "Success"'
  else
    echo -e "${RED}✗ Request failed${NC}"
    cat /tmp/test_response.json
  fi
) &

REQUEST_PID=$!

echo ""
echo -e "${YELLOW}Step 5: Waiting 5 seconds, then triggering graceful shutdown${NC}"
sleep 5

echo ""
echo -e "${YELLOW}Step 6: Sending SIGTERM to worker container${NC}"
docker-compose kill -s SIGTERM worker

echo ""
echo -e "${YELLOW}Step 7: Monitoring worker shutdown (checking logs)${NC}"
echo "Worker should:"
echo "  1. Set is_ready = false"
echo "  2. Cancel operations"
echo "  3. Wait for active requests"
echo "  4. Shutdown gracefully"
echo ""

# Show worker logs for 15 seconds
timeout 15 docker-compose logs -f worker 2>&1 | grep -E "(SIGTERM|graceful|shutdown|terminating|ready)" || true

echo ""
echo -e "${YELLOW}Step 8: Checking if request completed${NC}"
wait $REQUEST_PID 2>/dev/null || echo "Request process finished"

if [ -f /tmp/test_response.json ]; then
  echo ""
  echo "Response received:"
  cat /tmp/test_response.json | jq . 2>/dev/null || cat /tmp/test_response.json
fi

echo ""
echo -e "${YELLOW}Step 9: Cleanup${NC}"
docker-compose down

echo ""
echo -e "${GREEN}=== Test Complete ===${NC}"
echo ""
echo "Expected behavior:"
echo "  ✓ Worker receives SIGTERM"
echo "  ✓ Worker logs 'Received SIGTERM signal'"
echo "  ✓ Worker logs 'Worker marked as not ready'"
echo "  ✓ Worker logs 'Cancelling all active operations'"
echo "  ✓ Active requests complete or return TERMINATING"
echo "  ✓ Worker logs 'All active requests completed'"
echo "  ✓ Worker shuts down gracefully"
echo ""
echo "To test coordinator graceful shutdown, repeat with:"
echo "  docker-compose kill -s SIGTERM coordinator"
