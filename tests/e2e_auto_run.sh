#!/usr/bin/env bash
set -euo pipefail

# E2E Auto-Run Test — tests the POST /auto/:workflow endpoint
# Requires: CSVKEY, WORKER_URL env vars
# The Worker must have at least ANTHROPIC_API_KEY configured.
# Usage: CSVKEY=xxx WORKER_URL=http://localhost:8787 ANTHROPIC_MODEL=claude-... ./tests/e2e_auto_run.sh
#
# WARNING: This script makes real LLM API calls that cost money.

: "${CSVKEY:?Set CSVKEY env var}"
: "${WORKER_URL:?Set WORKER_URL env var}"
: "${ANTHROPIC_MODEL:=${ANTHROPIC_MODEL:-claude-sonnet-4-20250514}}"

PASS=0
FAIL=0
TOTAL=0
RUN_ID="$(date +%s)"
WF_NAME="e2e-autorun-${RUN_ID}"

log() { echo "[$(date +%H:%M:%S)] $*" >&2; }
api() { curl -s -o /tmp/e2e_auto_body -w '%{http_code}' "$@" 2>/dev/null; }

assert_status() {
    local desc="$1" expected="$2" actual="$3"
    TOTAL=$((TOTAL + 1))
    if [[ "$actual" == "$expected" ]]; then
        log "  PASS: $desc (status=$actual)"
        PASS=$((PASS + 1))
    else
        log "  FAIL: $desc — expected $expected, got $actual"
        log "    Body: $(head -c 500 /tmp/e2e_auto_body)"
        FAIL=$((FAIL + 1))
    fi
}

assert_json() {
    local desc="$1" expr="$2" expected="$3"
    TOTAL=$((TOTAL + 1))
    local actual
    actual=$(python3 -c "import json,sys; d=json.load(sys.stdin); print($expr)" < /tmp/e2e_auto_body 2>/dev/null || echo "PARSE_ERROR")
    if [[ "$actual" == "$expected" ]]; then
        log "  PASS: $desc ($actual)"
        PASS=$((PASS + 1))
    else
        log "  FAIL: $desc — expected '$expected', got '$actual'"
        FAIL=$((FAIL + 1))
    fi
}

cleanup() {
    log "Cleanup: deleting workflow $WF_NAME"
    curl -s -X DELETE "${WORKER_URL}/workflows/${WF_NAME}?csvkey=${CSVKEY}" > /dev/null 2>&1 || true
}
trap cleanup EXIT

log "=== E2E Auto-Run Tests ==="
log "Worker: $WORKER_URL"
log "Workflow: $WF_NAME"
log ""

# --- Setup: Upload docs and create workflow ---
log "Setup: Upload documents"
STATUS=$(api -X PUT -H "Content-Type: text/markdown" \
    -d "# Test README\n\nThis is a test project for auto-run E2E testing." \
    "${WORKER_URL}/documents/${WF_NAME}/readme?csvkey=${CSVKEY}")
assert_status "Upload readme doc" "200" "$STATUS"

STATUS=$(api -X PUT -H "Content-Type: text/markdown" \
    -d "# Test Spec\n\nThis specification describes a simple REST API with two endpoints:\n\n## GET /items\nReturns a list of items.\n\n## POST /items\nCreates a new item.\n\nItems have: id, name, description, created_at." \
    "${WORKER_URL}/documents/${WF_NAME}/spec?csvkey=${CSVKEY}")
assert_status "Upload spec doc" "200" "$STATUS"

log "Setup: Create workflow"
STATUS=$(api -X POST -H "Content-Type: application/json" \
    -d "{
        \"name\": \"${WF_NAME}\",
        \"provider\": \"anthropic\",
        \"model\": \"${ANTHROPIC_MODEL}\",
        \"system_prompt\": \"You are a brief code reviewer. Keep responses under 200 words.\",
        \"provider_params\": {\"max_tokens\": 500},
        \"documents\": {\"readme\": \"readme\", \"spec\": \"spec\"}
    }" \
    "${WORKER_URL}/workflows?csvkey=${CSVKEY}")
assert_status "Create workflow" "200" "$STATUS"

log ""

# --- Test 1: Input validation ---
log "Test 1: Input validation"

STATUS=$(api -X POST -H "Content-Type: application/json" \
    -d '{}' \
    "${WORKER_URL}/auto/${WF_NAME}?csvkey=${CSVKEY}")
assert_status "Missing rounds field → 400" "400" "$STATUS"

STATUS=$(api -X POST -H "Content-Type: application/json" \
    -d '{"rounds": 0}' \
    "${WORKER_URL}/auto/${WF_NAME}?csvkey=${CSVKEY}")
assert_status "rounds=0 → 400" "400" "$STATUS"

STATUS=$(api -X POST -H "Content-Type: application/json" \
    -d '{"rounds": 100}' \
    "${WORKER_URL}/auto/${WF_NAME}?csvkey=${CSVKEY}")
assert_status "rounds > MAX → 400" "400" "$STATUS"

STATUS=$(api -X POST -H "Content-Type: application/json" \
    -d '{"rounds": 3, "min_rounds": 0}' \
    "${WORKER_URL}/auto/${WF_NAME}?csvkey=${CSVKEY}")
assert_status "min_rounds=0 → 400" "400" "$STATUS"

STATUS=$(api -X POST -H "Content-Type: application/json" \
    -d '{"rounds": 3, "min_rounds": 5}' \
    "${WORKER_URL}/auto/${WF_NAME}?csvkey=${CSVKEY}")
assert_status "min_rounds > rounds → 400" "400" "$STATUS"

STATUS=$(api -X POST -H "Content-Type: application/json" \
    -d '{"rounds": 3, "convergence_threshold": 2.0}' \
    "${WORKER_URL}/auto/${WF_NAME}?csvkey=${CSVKEY}")
assert_status "threshold > 1.0 → 400" "400" "$STATUS"

STATUS=$(api -X POST -H "Content-Type: application/json" \
    -d '{"rounds": 3, "max_duration_seconds": 5}' \
    "${WORKER_URL}/auto/${WF_NAME}?csvkey=${CSVKEY}")
assert_status "max_duration < 30 → 400" "400" "$STATUS"

STATUS=$(api -X POST -H "Content-Type: application/json" \
    -d '{"rounds": 3, "provider": "openai", "provider_rotation": [{"provider":"anthropic","model":"x"}]}' \
    "${WORKER_URL}/auto/${WF_NAME}?csvkey=${CSVKEY}")
assert_status "rotation + provider → 400" "400" "$STATUS"

STATUS=$(api -X POST -H "Content-Type: application/json" \
    -d '{"rounds": 3, "provider_rotation": []}' \
    "${WORKER_URL}/auto/${WF_NAME}?csvkey=${CSVKEY}")
assert_status "empty rotation → 400" "400" "$STATUS"

STATUS=$(api -X GET "${WORKER_URL}/auto/${WF_NAME}?csvkey=${CSVKEY}")
assert_status "GET /auto → 405" "405" "$STATUS"

STATUS=$(api -X POST -H "Content-Type: application/json" \
    -d '{"rounds": 2}' \
    "${WORKER_URL}/auto/no-such-workflow-12345?csvkey=${CSVKEY}")
assert_status "nonexistent workflow → 404" "404" "$STATUS"

log ""

# --- Test 2: JSON auto-run (2 rounds) ---
log "Test 2: JSON auto-run (2 rounds with Anthropic)"
STATUS=$(api -X POST \
    -H "Content-Type: application/json" \
    -H "Accept: application/json" \
    -d "{\"rounds\": 2, \"stop_on_convergence\": false, \"provider_params\": {\"max_tokens\": 500}}" \
    "${WORKER_URL}/auto/${WF_NAME}?csvkey=${CSVKEY}")
assert_status "Auto-run 2 rounds → 200" "200" "$STATUS"
assert_json "ok=true" "d['ok']" "True"
assert_json "rounds_completed=2" "d['data']['rounds_completed']" "2"
assert_json "stopped_reason=completed" "d['data']['stopped_reason']" "completed"
assert_json "start_round=1" "d['data']['start_round']" "1"
assert_json "final_round_number=2" "d['data']['final_round_number']" "2"
assert_json "has rounds_summary" "len(d['data']['rounds_summary'])" "2"
assert_json "summary[0] has words" "d['data']['rounds_summary'][0]['words'] > 0" "True"
assert_json "has total_usage" "'total_usage' in d['data']" "True"
assert_json "total_duration > 0" "d['data']['total_duration_seconds'] >= 0" "True"

log ""

# --- Test 3: Verify rounds are persisted in KV ---
log "Test 3: Verify persisted rounds"
STATUS=$(api "${WORKER_URL}/rounds/${WF_NAME}/1?csvkey=${CSVKEY}")
assert_status "GET round 1 → 200" "200" "$STATUS"
assert_json "round 1 complete" "d['data']['status']" "complete"

STATUS=$(api "${WORKER_URL}/rounds/${WF_NAME}/2?csvkey=${CSVKEY}")
assert_status "GET round 2 → 200" "200" "$STATUS"
assert_json "round 2 complete" "d['data']['status']" "complete"

log ""

# --- Test 4: Resume auto-run (should start at round 3) ---
log "Test 4: Resume auto-run (start at round 3)"
STATUS=$(api -X POST \
    -H "Content-Type: application/json" \
    -H "Accept: application/json" \
    -d "{\"rounds\": 1, \"stop_on_convergence\": false, \"provider_params\": {\"max_tokens\": 500}}" \
    "${WORKER_URL}/auto/${WF_NAME}?csvkey=${CSVKEY}")
assert_status "Resume auto-run → 200" "200" "$STATUS"
assert_json "start_round=3" "d['data']['start_round']" "3"
assert_json "rounds_completed=1" "d['data']['rounds_completed']" "1"
assert_json "final_round=3" "d['data']['final_round_number']" "3"

log ""

# --- Test 5: SSE auto-run (1 round) ---
log "Test 5: SSE auto-run (1 round)"
SSE_OUTPUT=$(curl -s -N -X POST \
    -H "Content-Type: application/json" \
    -d "{\"rounds\": 1, \"stop_on_convergence\": false, \"provider_params\": {\"max_tokens\": 500}}" \
    "${WORKER_URL}/auto/${WF_NAME}?csvkey=${CSVKEY}" 2>/dev/null)

TOTAL=$((TOTAL + 1))
if echo "$SSE_OUTPUT" | grep -q "event: round_start"; then
    log "  PASS: SSE has round_start event"
    PASS=$((PASS + 1))
else
    log "  FAIL: SSE missing round_start event"
    FAIL=$((FAIL + 1))
fi

TOTAL=$((TOTAL + 1))
if echo "$SSE_OUTPUT" | grep -q "event: round_complete"; then
    log "  PASS: SSE has round_complete event"
    PASS=$((PASS + 1))
else
    log "  FAIL: SSE missing round_complete event"
    FAIL=$((FAIL + 1))
fi

TOTAL=$((TOTAL + 1))
if echo "$SSE_OUTPUT" | grep -q "event: done"; then
    log "  PASS: SSE has done event"
    PASS=$((PASS + 1))
else
    log "  FAIL: SSE missing done event"
    FAIL=$((FAIL + 1))
fi

log ""

# --- Test 6: include_integration ---
log "Test 6: include_integration"
STATUS=$(api -X POST \
    -H "Content-Type: application/json" \
    -H "Accept: application/json" \
    -d "{\"rounds\": 1, \"stop_on_convergence\": false, \"include_integration\": true, \"provider_params\": {\"max_tokens\": 500}}" \
    "${WORKER_URL}/auto/${WF_NAME}?csvkey=${CSVKEY}")
assert_status "Auto-run with integration → 200" "200" "$STATUS"
assert_json "has integration_prompt" "'integration_prompt' in d['data']" "True"
assert_json "prompt not empty" "len(d['data']['integration_prompt']) > 0" "True"

log ""

# --- Test 7: Stats verification ---
log "Test 7: Stats after auto-run"
STATUS=$(api "${WORKER_URL}/stats/${WF_NAME}?csvkey=${CSVKEY}")
assert_status "GET stats → 200" "200" "$STATUS"
assert_json "has rounds in stats" "d['data']['total_rounds'] > 0" "True"

log ""
log "=== Results: $PASS passed, $FAIL failed (out of $TOTAL) ==="
[ "$FAIL" -eq 0 ] && log "All checks passed!" || log "Some checks failed."
exit "$FAIL"
