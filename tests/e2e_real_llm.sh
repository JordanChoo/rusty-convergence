#!/usr/bin/env bash
set -euo pipefail

# E2E Real LLM Test — tests against real OpenAI/Anthropic APIs
# Requires: CSVKEY, OPENAI_API_KEY, ANTHROPIC_API_KEY, WORKER_URL env vars
# Usage: CSVKEY=xxx WORKER_URL=https://your-worker.dev ./tests/e2e_real_llm.sh
#
# WARNING: This script makes real LLM API calls that cost money.
# Each run costs approximately $0.10-1.00 depending on model pricing.

: "${CSVKEY:?Set CSVKEY env var}"
: "${WORKER_URL:?Set WORKER_URL env var}"
: "${OPENAI_API_KEY:?Set OPENAI_API_KEY env var (needed by the Worker, not this script)}"

PASS=0
FAIL=0
TOTAL=0
WF_NAME="e2e-test-$(date +%s)"
LOGDIR="logs"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
LOGFILE="${LOGDIR}/e2e_llm_${TIMESTAMP}.log"
mkdir -p "$LOGDIR"

log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$LOGFILE"; }
api() { curl -s -o /tmp/e2e_body -w '%{http_code}' "$@" 2>>"$LOGFILE"; }

assert_status() {
    local desc="$1" expected="$2" actual="$3"
    TOTAL=$((TOTAL + 1))
    if [[ "$actual" == "$expected" ]]; then
        log "  PASS: $desc (status=$actual)"
        PASS=$((PASS + 1))
    else
        log "  FAIL: $desc — expected $expected, got $actual"
        log "    Body: $(cat /tmp/e2e_body | head -c 500)"
        FAIL=$((FAIL + 1))
    fi
}

assert_json_field() {
    local desc="$1" field="$2" expected="$3"
    TOTAL=$((TOTAL + 1))
    local actual
    actual=$(cat /tmp/e2e_body | python3 -c "import json,sys; d=json.load(sys.stdin); print(d${field})" 2>/dev/null || echo "PARSE_ERROR")
    if [[ "$actual" == "$expected" ]]; then
        log "  PASS: $desc ($field=$actual)"
        PASS=$((PASS + 1))
    else
        log "  FAIL: $desc — $field expected '$expected', got '$actual'"
        FAIL=$((FAIL + 1))
    fi
}

assert_json_nonempty() {
    local desc="$1" field="$2"
    TOTAL=$((TOTAL + 1))
    local actual
    actual=$(cat /tmp/e2e_body | python3 -c "
import json,sys
d=json.load(sys.stdin)
v=d${field}
print('nonempty' if v and str(v).strip() else 'empty')
" 2>/dev/null || echo "PARSE_ERROR")
    if [[ "$actual" == "nonempty" ]]; then
        log "  PASS: $desc ($field is non-empty)"
        PASS=$((PASS + 1))
    else
        log "  FAIL: $desc — $field is empty or missing"
        FAIL=$((FAIL + 1))
    fi
}

cleanup() {
    log "Cleanup: DELETE /workflows/${WF_NAME}"
    api -X DELETE "${WORKER_URL}/workflows/${WF_NAME}?csvkey=${CSVKEY}"
    log "  Cleanup done (status=$(cat /tmp/e2e_body | python3 -c "import json,sys; print(json.load(sys.stdin).get('code','?'))" 2>/dev/null || echo '?'))"
}
trap cleanup EXIT

log "=== E2E Real LLM Test ==="
log "Worker: $WORKER_URL"
log "Workflow: $WF_NAME"
log ""

# Step 1: Upload documents
log "Step 1: Upload README"
STATUS=$(api -X PUT -H "Content-Type: text/markdown" \
  -d "# Test Project\n\nA small test project for validating the APRP Worker.\nIt has a simple API with two endpoints." \
  "${WORKER_URL}/documents/${WF_NAME}/readme?csvkey=${CSVKEY}")
assert_status "PUT /documents/readme" "200" "$STATUS"

log "Step 2: Upload spec"
STATUS=$(api -X PUT -H "Content-Type: text/markdown" \
  -d "# Test Specification\n\n## Overview\nThis spec defines a REST API.\n\n## Endpoints\n- GET /items — list items\n- POST /items — create item\n\n## Data Model\nItems have: id, name, created_at.\n\n## Security\nBearer token auth required." \
  "${WORKER_URL}/documents/${WF_NAME}/spec?csvkey=${CSVKEY}")
assert_status "PUT /documents/spec" "200" "$STATUS"

# Step 3: Create workflow
log "Step 3: Create workflow"
STATUS=$(api -X POST -H "Content-Type: application/json" \
  -d "{\"name\":\"${WF_NAME}\",\"provider\":\"openai\",\"model\":\"o3-mini\",\"provider_params\":{\"max_completion_tokens\":1000}}" \
  "${WORKER_URL}/workflows?csvkey=${CSVKEY}")
assert_status "POST /workflows" "200" "$STATUS"

# Step 4: Run round 1 (OpenAI)
log "Step 4: Run round 1 (OpenAI o3-mini) — this may take 30-120 seconds"
STATUS=$(api --max-time 300 -X POST -H "Content-Type: application/json" \
  -d "{}" \
  "${WORKER_URL}/run/${WF_NAME}/1?csvkey=${CSVKEY}")
assert_status "POST /run round 1" "200" "$STATUS"
assert_json_field "Round 1 status" "['data']['status']" "complete"
assert_json_nonempty "Round 1 content" "['data']['content']"
assert_json_nonempty "Round 1 words" "['data']['metrics']['words']"

# Step 5: GET round 1
log "Step 5: GET round 1"
STATUS=$(api "${WORKER_URL}/rounds/${WF_NAME}/1?csvkey=${CSVKEY}")
assert_status "GET /rounds/1" "200" "$STATUS"
assert_json_field "Stored round 1 status" "['data']['status']" "complete"

# Step 6: Run round 2
log "Step 6: Run round 2 (OpenAI o3-mini) — this may take 30-120 seconds"
STATUS=$(api --max-time 300 -X POST -H "Content-Type: application/json" \
  -d "{}" \
  "${WORKER_URL}/run/${WF_NAME}/2?csvkey=${CSVKEY}")
assert_status "POST /run round 2" "200" "$STATUS"
assert_json_field "Round 2 status" "['data']['status']" "complete"
assert_json_nonempty "Round 2 convergence score" "['data']['convergence']['score']"

# Step 7: GET stats
log "Step 7: GET /stats"
STATUS=$(api "${WORKER_URL}/stats/${WF_NAME}?csvkey=${CSVKEY}")
assert_status "GET /stats" "200" "$STATUS"
assert_json_nonempty "Stats total_rounds" "['data']['total_rounds']"

# Step 8: GET round list
log "Step 8: GET /rounds (list)"
STATUS=$(api "${WORKER_URL}/rounds/${WF_NAME}?csvkey=${CSVKEY}")
assert_status "GET /rounds list" "200" "$STATUS"

# Step 9: POST integrate
log "Step 9: POST /integrate round 2"
STATUS=$(api -X POST "${WORKER_URL}/integrate/${WF_NAME}/2?csvkey=${CSVKEY}")
assert_status "POST /integrate" "200" "$STATUS"
assert_json_nonempty "Integration prompt" "['data']['prompt']"

log ""
log "=== SUMMARY ==="
log "Total: $TOTAL  Passed: $PASS  Failed: $FAIL"
log "Log: $LOGFILE"

if [[ $FAIL -gt 0 ]]; then
    exit 1
fi
exit 0
