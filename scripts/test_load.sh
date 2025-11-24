#!/bin/bash

URL="http://127.0.0.1:3000"
GREEN='\033[0;32m'
RED='\033[0;31m'
BLUE='\033[0;34m'
NC='\033[0m'

echo -e "${BLUE}=================================================${NC}"
echo -e "${BLUE}       🧪 OBSERVABILITY & LLM LOAD TEST         ${NC}"
echo -e "${BLUE}=================================================${NC}"

# 1. HTTP & Error Tracking
echo -e "\n${GREEN}[1/4] Generating HTTP Traffic (Targeting ~15 Errors)...${NC}"
# Increased to 50 to guarantee ~15 failures (30% rate)
for i in {1..50}; do
    curl -s -X POST "$URL/order" -H "Content-Type: application/json" > /dev/null &
done
wait
echo "  -> Done."

# 2. Active Connections (SSE)
echo -e "\n${GREEN}[2/4] Starting SSE Stream...${NC}"
curl -N "$URL/prices" > /dev/null 2>&1 &
SSE_PID=$!
sleep 2

# 3. OpenAI Integration (LLM)
echo -e "\n${GREEN}[3/4] Testing OpenAI Integration...${NC}"

# Start Stream in Background
echo "  -> Sending Stream Request (This takes ~5-10 seconds)..."
curl -s -X POST "$URL/chat/stream" -H "Content-Type: application/json" > stream_output.json &
STREAM_PID=$!

# Start Unary in Background
echo "  -> Sending Unary Request..."
curl -s -X POST "$URL/chat/ask" -H "Content-Type: application/json" > unary_output.json &
UNARY_PID=$!

# Wait for both
wait $STREAM_PID
echo "  -> Stream finished."
wait $UNARY_PID
echo "  -> Unary finished."

# 4. Metrics Verification
echo -e "\n${GREEN}[4/4] 📸 Verifying Metrics...${NC}"
sleep 1 # Give metrics exporter a moment to scrape/update
METRICS=$(curl -s "$URL/metrics")

# Robust check function (Order Independent)
check_labels() {
  local metric="$1" ; shift
  local human="$1"  ; shift
  
  # 1. Check if metric name exists
  if ! echo "$METRICS" | grep -q "$metric"; then
     echo -e "  ❌ ${RED}Missing Metric Name: $metric ($human)${NC}"
     return
  fi

  # 2. Check if ALL labels exist on the same line as that metric
  # We grep for the metric name, then pipe to grep for each label individually
  local matching_line=$(echo "$METRICS" | grep "$metric")
  
  local all_found=true
  for label in "$@"; do
    if ! echo "$matching_line" | grep -q "$label"; then
        all_found=false
    fi
  done

  if [ "$all_found" = true ]; then
    echo -e "  ✅ Found: $human"
  else
    echo -e "  ❌ ${RED}Missing Labels for: $human${NC}"
    echo "     Debug: Searched for $@ in $metric"
  fi
}

echo "-------------------------------------------------"
# Core Metrics
check_labels 'http_requests_active' "Active SSE Connection" 'type="sse"'
# Note: We use specific counters, not regex start anchors, to be safer
check_labels 'function_errors_total' "Business Logic Errors" 'function="process_payment"' 'service="order_service"'
check_labels 'function_errors_total' "Background Task Errors" 'function="email_user"' 'service="order_service"'

# LLM Metrics
check_labels 'external_request_duration_seconds_ms_count' "LLM Unary Latency" 'target="gpt_unary"' 'method="POST"' 'status="completed"'
check_labels 'external_request_duration_seconds_ms_count' "LLM Stream Handshake" 'target="gpt_handshake"' 'method="POST"' 'status="completed"'
check_labels 'external_stream_duration_seconds_ms_count' "LLM Stream Generation" 'service="gpt_generation"'
echo "-------------------------------------------------"

# Cleanup
kill $SSE_PID 2>/dev/null
echo -e "${BLUE}Test Complete.${NC}"

echo "${METRICS}"