#!/usr/bin/env bash
set -euo pipefail

# E2E Real LLM Test — tests against real OpenAI/Anthropic APIs
# Requires: CSVKEY, OPENAI_API_KEY, ANTHROPIC_API_KEY, ANTHROPIC_MODEL, WORKER_URL env vars
# Usage: CSVKEY=xxx WORKER_URL=https://your-worker.dev ANTHROPIC_MODEL=claude-... ./tests/e2e_real_llm.sh
#
# WARNING: This script makes real LLM API calls that cost money.
# Each run costs approximately $0.10-1.00 depending on model pricing.

: "${CSVKEY:?Set CSVKEY env var}"
: "${WORKER_URL:?Set WORKER_URL env var}"
: "${OPENAI_API_KEY:?Set OPENAI_API_KEY env var (needed by the Worker, not this script)}"
: "${ANTHROPIC_API_KEY:?Set ANTHROPIC_API_KEY env var (needed by the Worker, not this script)}"
: "${ANTHROPIC_MODEL:?Set ANTHROPIC_MODEL env var (Claude model enabled for your account)}"

OPENAI_MODEL="${OPENAI_MODEL:-o3-mini}"
OPENAI_MAX_COMPLETION_TOKENS="${OPENAI_MAX_COMPLETION_TOKENS:-1000}"
ANTHROPIC_MAX_TOKENS="${ANTHROPIC_MAX_TOKENS:-1000}"

PASS=0
FAIL=0
TOTAL=0
RUN_ID="$(date +%s)"
OPENAI_WF_NAME="e2e-test-openai-${RUN_ID}"
ANTHROPIC_WF_NAME="e2e-test-anthropic-${RUN_ID}"
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
        log "    Body: $(head -c 500 /tmp/e2e_body)"
        FAIL=$((FAIL + 1))
    fi
}

assert_json_field() {
    local desc="$1" field="$2" expected="$3"
    TOTAL=$((TOTAL + 1))
    local actual
    actual=$(python3 -c "import json,sys; d=json.load(sys.stdin); print(d${field})" < /tmp/e2e_body 2>/dev/null || echo "PARSE_ERROR")
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
    actual=$(python3 -c "
import json,sys
d=json.load(sys.stdin)
v=d${field}
print('nonempty' if v and str(v).strip() else 'empty')
" < /tmp/e2e_body 2>/dev/null || echo "PARSE_ERROR")
    if [[ "$actual" == "nonempty" ]]; then
        log "  PASS: $desc ($field is non-empty)"
        PASS=$((PASS + 1))
    else
        log "  FAIL: $desc — $field is empty or missing"
        FAIL=$((FAIL + 1))
    fi
}

# shellcheck disable=SC2317
cleanup() {
    for wf in "$OPENAI_WF_NAME" "$ANTHROPIC_WF_NAME"; do
        log "Cleanup: DELETE /workflows/${wf}"
        api -X DELETE "${WORKER_URL}/workflows/${wf}?csvkey=${CSVKEY}"
        log "  Cleanup done (status=$(python3 -c "import json,sys; print(json.load(sys.stdin).get('code','?'))" < /tmp/e2e_body 2>/dev/null || echo '?'))"
    done
}
trap cleanup EXIT

upload_docs() {
    local wf_name="$1"

    log "Upload README (${wf_name})"
    STATUS=$(api -X PUT -H "Content-Type: text/markdown" \
      -d "# Test Project\n\nA small test project for validating the APRP Worker.\nIt has a simple API with two endpoints." \
      "${WORKER_URL}/documents/${wf_name}/readme?csvkey=${CSVKEY}")
    assert_status "PUT /documents/readme (${wf_name})" "200" "$STATUS"

    log "Upload spec (${wf_name})"
    STATUS=$(api -X PUT -H "Content-Type: text/markdown" \
      -d "# Test Specification\n\n## Overview\nThis spec defines a REST API.\n\n## Endpoints\n- GET /items — list items\n- POST /items — create item\n\n## Data Model\nItems have: id, name, created_at.\n\n## Security\nBearer token auth required." \
      "${WORKER_URL}/documents/${wf_name}/spec?csvkey=${CSVKEY}")
    assert_status "PUT /documents/spec (${wf_name})" "200" "$STATUS"
}

log "=== E2E Real LLM Test ==="
log "Worker: $WORKER_URL"
log "OpenAI workflow: $OPENAI_WF_NAME"
log "Anthropic workflow: $ANTHROPIC_WF_NAME"
log ""

# Step 1: Upload OpenAI documents
log "Step 1: Upload OpenAI documents"
upload_docs "$OPENAI_WF_NAME"

# Step 2: Create OpenAI workflow
log "Step 2: Create OpenAI workflow"
STATUS=$(api -X POST -H "Content-Type: application/json" \
  -d "{\"name\":\"${OPENAI_WF_NAME}\",\"provider\":\"openai\",\"model\":\"${OPENAI_MODEL}\",\"provider_params\":{\"max_completion_tokens\":${OPENAI_MAX_COMPLETION_TOKENS}}}" \
  "${WORKER_URL}/workflows?csvkey=${CSVKEY}")
assert_status "POST /workflows (OpenAI)" "200" "$STATUS"

# Step 3: Run OpenAI round 1
log "Step 3: Run round 1 (OpenAI ${OPENAI_MODEL}) — this may take 30-120 seconds"
STATUS=$(api --max-time 300 -X POST -H "Content-Type: application/json" \
  -d "{}" \
  "${WORKER_URL}/run/${OPENAI_WF_NAME}/1?csvkey=${CSVKEY}")
assert_status "POST /run OpenAI round 1" "200" "$STATUS"
assert_json_field "Round 1 status" "['data']['status']" "complete"
assert_json_nonempty "Round 1 content" "['data']['content']"
assert_json_nonempty "Round 1 words" "['data']['metrics']['words']"

# Step 4: GET OpenAI round 1
log "Step 4: GET OpenAI round 1"
STATUS=$(api "${WORKER_URL}/rounds/${OPENAI_WF_NAME}/1?csvkey=${CSVKEY}")
assert_status "GET /rounds OpenAI round 1" "200" "$STATUS"
assert_json_field "Stored round 1 status" "['data']['status']" "complete"

# Step 5: Run OpenAI round 2
log "Step 5: Run round 2 (OpenAI ${OPENAI_MODEL}) — this may take 30-120 seconds"
STATUS=$(api --max-time 300 -X POST -H "Content-Type: application/json" \
  -d "{}" \
  "${WORKER_URL}/run/${OPENAI_WF_NAME}/2?csvkey=${CSVKEY}")
assert_status "POST /run OpenAI round 2" "200" "$STATUS"
assert_json_field "Round 2 status" "['data']['status']" "complete"
assert_json_nonempty "Round 2 convergence score" "['data']['convergence']['score']"

# Step 6: GET OpenAI stats
log "Step 6: GET OpenAI /stats"
STATUS=$(api "${WORKER_URL}/stats/${OPENAI_WF_NAME}?csvkey=${CSVKEY}")
assert_status "GET /stats OpenAI" "200" "$STATUS"
assert_json_nonempty "Stats total_rounds" "['data']['total_rounds']"

# Step 7: GET OpenAI round list
log "Step 7: GET OpenAI /rounds (list)"
STATUS=$(api "${WORKER_URL}/rounds/${OPENAI_WF_NAME}?csvkey=${CSVKEY}")
assert_status "GET /rounds list OpenAI" "200" "$STATUS"

# Step 8: POST OpenAI integrate
log "Step 8: POST OpenAI /integrate round 2"
STATUS=$(api -X POST "${WORKER_URL}/integrate/${OPENAI_WF_NAME}/2?csvkey=${CSVKEY}")
assert_status "POST /integrate OpenAI" "200" "$STATUS"
assert_json_nonempty "Integration prompt" "['data']['prompt']"

# Step 9: Upload Anthropic documents
log "Step 9: Upload Anthropic documents"
upload_docs "$ANTHROPIC_WF_NAME"

# Step 10: Create Anthropic workflow
log "Step 10: Create Anthropic workflow"
STATUS=$(api -X POST -H "Content-Type: application/json" \
  -d "{\"name\":\"${ANTHROPIC_WF_NAME}\",\"provider\":\"anthropic\",\"model\":\"${ANTHROPIC_MODEL}\",\"provider_params\":{\"max_tokens\":${ANTHROPIC_MAX_TOKENS}}}" \
  "${WORKER_URL}/workflows?csvkey=${CSVKEY}")
assert_status "POST /workflows (Anthropic)" "200" "$STATUS"

# Step 11: Run Anthropic round 1
log "Step 11: Run round 1 (Anthropic ${ANTHROPIC_MODEL}) — this may take 30-120 seconds"
STATUS=$(api --max-time 300 -X POST -H "Content-Type: application/json" \
  -d "{}" \
  "${WORKER_URL}/run/${ANTHROPIC_WF_NAME}/1?csvkey=${CSVKEY}")
assert_status "POST /run Anthropic round 1" "200" "$STATUS"
assert_json_field "Anthropic round 1 status" "['data']['status']" "complete"
assert_json_nonempty "Anthropic round 1 content" "['data']['content']"
assert_json_nonempty "Anthropic round 1 words" "['data']['metrics']['words']"

# Step 12: GET Anthropic round 1
log "Step 12: GET Anthropic round 1"
STATUS=$(api "${WORKER_URL}/rounds/${ANTHROPIC_WF_NAME}/1?csvkey=${CSVKEY}")
assert_status "GET /rounds Anthropic round 1" "200" "$STATUS"
assert_json_field "Stored Anthropic round 1 status" "['data']['status']" "complete"

# Step 13: GET Anthropic stats
log "Step 13: GET Anthropic /stats"
STATUS=$(api "${WORKER_URL}/stats/${ANTHROPIC_WF_NAME}?csvkey=${CSVKEY}")
assert_status "GET /stats Anthropic" "200" "$STATUS"
assert_json_nonempty "Anthropic stats total_rounds" "['data']['total_rounds']"

log ""
log "=== SUMMARY ==="
log "Total: $TOTAL  Passed: $PASS  Failed: $FAIL"
log "Log: $LOGFILE"

if [[ $FAIL -gt 0 ]]; then
    exit 1
fi
exit 0
