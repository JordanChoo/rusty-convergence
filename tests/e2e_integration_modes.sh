#!/usr/bin/env bash
set -euo pipefail

# E2E Integration Modes Test — tests Claude and human document mutation
# Requires: CSVKEY, WORKER_URL env vars
# The Worker must have ANTHROPIC_API_KEY configured (used for both review and integration).
# Usage: CSVKEY=xxx WORKER_URL=https://your-worker.dev ./tests/e2e_integration_modes.sh
#
# WARNING: This script makes real LLM API calls that cost money.
# Claude integration mode makes extra Anthropic calls per round per document.

: "${CSVKEY:?Set CSVKEY env var}"
: "${WORKER_URL:?Set WORKER_URL env var}"
: "${ANTHROPIC_MODEL:=${ANTHROPIC_MODEL:-claude-sonnet-4-20250514}}"
: "${INTEGRATION_MODEL:=${INTEGRATION_MODEL:-claude-sonnet-4-6}}"

PASS=0
FAIL=0
TOTAL=0
RUN_ID="$(date +%s)"
WF_CLAUDE="e2e-int-claude-${RUN_ID}"
WF_HUMAN="e2e-int-human-${RUN_ID}"

log() { echo "[$(date +%H:%M:%S)] $*" >&2; }
api() { curl -s -o /tmp/e2e_int_body -w '%{http_code}' "$@" 2>/dev/null; }
body() { cat /tmp/e2e_int_body; }

assert_status() {
    local desc="$1" expected="$2" actual="$3"
    TOTAL=$((TOTAL + 1))
    if [[ "$actual" == "$expected" ]]; then
        log "  PASS: $desc (status=$actual)"
        PASS=$((PASS + 1))
    else
        log "  FAIL: $desc — expected $expected, got $actual"
        log "    Body: $(head -c 500 /tmp/e2e_int_body)"
        FAIL=$((FAIL + 1))
    fi
}

assert_json() {
    local desc="$1" expr="$2" expected="$3"
    TOTAL=$((TOTAL + 1))
    local actual
    actual=$(python3 -c "import json,sys; d=json.load(sys.stdin); print($expr)" < /tmp/e2e_int_body 2>/dev/null || echo "PARSE_ERROR")
    if [[ "$actual" == "$expected" ]]; then
        log "  PASS: $desc ($actual)"
        PASS=$((PASS + 1))
    else
        log "  FAIL: $desc — expected '$expected', got '$actual'"
        FAIL=$((FAIL + 1))
    fi
}

assert_contains() {
    local desc="$1" needle="$2" haystack="$3"
    TOTAL=$((TOTAL + 1))
    if echo "$haystack" | grep -q "$needle"; then
        log "  PASS: $desc"
        PASS=$((PASS + 1))
    else
        log "  FAIL: $desc — '$needle' not found"
        FAIL=$((FAIL + 1))
    fi
}

cleanup() {
    log "Cleanup: deleting workflows"
    curl -s -X DELETE "${WORKER_URL}/workflows/${WF_CLAUDE}?csvkey=${CSVKEY}" > /dev/null 2>&1 || true
    curl -s -X DELETE "${WORKER_URL}/workflows/${WF_HUMAN}?csvkey=${CSVKEY}" > /dev/null 2>&1 || true
}
trap cleanup EXIT

SPEC_V1="# API Spec v1

## Endpoints
GET /items returns all items.
POST /items creates an item.

## Auth
No authentication.

## Errors
Returns 500 on failure."

log "=== E2E Integration Modes Tests ==="
log "Worker: $WORKER_URL"
log "Claude workflow: $WF_CLAUDE"
log "Human workflow: $WF_HUMAN"
log ""

# ============================================================
# Validation tests (no LLM calls)
# ============================================================

log "Test 1: Integration mode validation"

# Set up a minimal workflow for validation tests
STATUS=$(api -X PUT -H "Content-Type: text/markdown" \
    -d "# README" \
    "${WORKER_URL}/documents/${WF_CLAUDE}/readme?csvkey=${CSVKEY}")
assert_status "Upload readme" "200" "$STATUS"

STATUS=$(api -X PUT -H "Content-Type: text/markdown" \
    -d "$SPEC_V1" \
    "${WORKER_URL}/documents/${WF_CLAUDE}/spec?csvkey=${CSVKEY}")
assert_status "Upload spec" "200" "$STATUS"

STATUS=$(api -X POST -H "Content-Type: application/json" \
    -d "{
        \"name\": \"${WF_CLAUDE}\",
        \"provider\": \"anthropic\",
        \"model\": \"${ANTHROPIC_MODEL}\",
        \"system_prompt\": \"You are a brief spec reviewer. Keep output under 300 words.\",
        \"provider_params\": {\"max_tokens\": 800},
        \"documents\": {\"readme\": \"readme\", \"spec\": \"spec\"}
    }" \
    "${WORKER_URL}/workflows?csvkey=${CSVKEY}")
assert_status "Create workflow" "200" "$STATUS"

# Invalid integration_mode
STATUS=$(api -X POST -H "Content-Type: application/json" -H "Accept: application/json" \
    -d '{"rounds": 1, "integration_mode": "auto"}' \
    "${WORKER_URL}/auto/${WF_CLAUDE}?csvkey=${CSVKEY}")
assert_status "Invalid integration_mode → 400" "400" "$STATUS"
assert_json "Error mentions integration_mode" "'integration_mode' in d.get('error','')" "True"

# Human mode without Accept: application/json (SSE default)
STATUS=$(api -X POST -H "Content-Type: application/json" \
    -d '{"rounds": 1, "integration_mode": "human"}' \
    "${WORKER_URL}/auto/${WF_CLAUDE}?csvkey=${CSVKEY}")
assert_status "Human mode without JSON accept → 400" "400" "$STATUS"

# integration_mode=none is valid (backward compat)
STATUS=$(api -X POST -H "Content-Type: application/json" -H "Accept: application/json" \
    -d '{"rounds": 1, "integration_mode": "none", "stop_on_convergence": false, "provider_params": {"max_tokens": 500}}' \
    "${WORKER_URL}/auto/${WF_CLAUDE}?csvkey=${CSVKEY}")
assert_status "integration_mode=none → 200" "200" "$STATUS"
assert_json "No integration_mode in response" "d['data'].get('integration_mode') is None" "True"

# Resume without pending state → 404
STATUS=$(api -X POST -H "Accept: application/json" \
    "${WORKER_URL}/auto/${WF_CLAUDE}/resume?csvkey=${CSVKEY}")
assert_status "Resume with no pending state → 404" "404" "$STATUS"

log ""

# ============================================================
# Claude integration mode (real LLM calls)
# ============================================================

log "Test 2: Claude integration mode (2 rounds)"

# Capture spec before integration
SPEC_BEFORE=$(curl -s "${WORKER_URL}/documents/${WF_CLAUDE}/spec?csvkey=${CSVKEY}")
log "  Spec before: $(echo "$SPEC_BEFORE" | wc -w | tr -d ' ') words"

STATUS=$(api -X POST -H "Content-Type: application/json" -H "Accept: application/json" \
    -d "{
        \"rounds\": 2,
        \"stop_on_convergence\": false,
        \"integration_mode\": \"claude\",
        \"integration_model\": \"${INTEGRATION_MODEL}\",
        \"provider_params\": {\"max_tokens\": 800}
    }" \
    "${WORKER_URL}/auto/${WF_CLAUDE}?csvkey=${CSVKEY}")
assert_status "Claude integration auto-run → 200" "200" "$STATUS"
assert_json "ok=true" "d['ok']" "True"
assert_json "rounds_completed=2" "d['data']['rounds_completed']" "2"
assert_json "stopped_reason=completed" "d['data']['stopped_reason']" "completed"
assert_json "integration_mode=claude" "d['data']['integration_mode']" "claude"
assert_json "has integration_usage" "'integration_usage' in d['data']" "True"
assert_json "integration used input tokens" "d['data']['integration_usage']['input_tokens'] > 0" "True"
assert_json "integration used output tokens" "d['data']['integration_usage']['output_tokens'] > 0" "True"

# Verify spec was actually mutated
SPEC_AFTER=$(curl -s "${WORKER_URL}/documents/${WF_CLAUDE}/spec?csvkey=${CSVKEY}")
WORDS_BEFORE=$(echo "$SPEC_BEFORE" | wc -w | tr -d ' ')
WORDS_AFTER=$(echo "$SPEC_AFTER" | wc -w | tr -d ' ')
log "  Spec after: ${WORDS_AFTER} words (was ${WORDS_BEFORE})"

TOTAL=$((TOTAL + 1))
if [[ "$SPEC_BEFORE" != "$SPEC_AFTER" ]]; then
    log "  PASS: Spec document was mutated by integration"
    PASS=$((PASS + 1))
else
    log "  FAIL: Spec document unchanged after Claude integration"
    FAIL=$((FAIL + 1))
fi

log ""

# ============================================================
# Claude integration SSE events
# ============================================================

log "Test 3: Claude integration SSE events"
SSE_OUTPUT=$(curl -s -N -X POST \
    -H "Content-Type: application/json" \
    -d "{
        \"rounds\": 2,
        \"stop_on_convergence\": false,
        \"integration_mode\": \"claude\",
        \"integration_model\": \"${INTEGRATION_MODEL}\",
        \"provider_params\": {\"max_tokens\": 500}
    }" \
    "${WORKER_URL}/auto/${WF_CLAUDE}?csvkey=${CSVKEY}" 2>/dev/null)

assert_contains "SSE has integration_start event" "event: integration_start" "$SSE_OUTPUT"
assert_contains "SSE has integration_complete event" "event: integration_complete" "$SSE_OUTPUT"
assert_contains "SSE has round_start event" "event: round_start" "$SSE_OUTPUT"
assert_contains "SSE has done event" "event: done" "$SSE_OUTPUT"

log ""

# ============================================================
# Human integration mode
# ============================================================

log "Test 4: Human integration mode setup"

# Create a separate workflow for human mode
STATUS=$(api -X PUT -H "Content-Type: text/markdown" \
    -d "# README" \
    "${WORKER_URL}/documents/${WF_HUMAN}/readme?csvkey=${CSVKEY}")
assert_status "Upload human readme" "200" "$STATUS"

STATUS=$(api -X PUT -H "Content-Type: text/markdown" \
    -d "$SPEC_V1" \
    "${WORKER_URL}/documents/${WF_HUMAN}/spec?csvkey=${CSVKEY}")
assert_status "Upload human spec" "200" "$STATUS"

STATUS=$(api -X POST -H "Content-Type: application/json" \
    -d "{
        \"name\": \"${WF_HUMAN}\",
        \"provider\": \"anthropic\",
        \"model\": \"${ANTHROPIC_MODEL}\",
        \"system_prompt\": \"You are a brief spec reviewer. Keep output under 300 words.\",
        \"provider_params\": {\"max_tokens\": 800},
        \"documents\": {\"readme\": \"readme\", \"spec\": \"spec\"}
    }" \
    "${WORKER_URL}/workflows?csvkey=${CSVKEY}")
assert_status "Create human workflow" "200" "$STATUS"

log ""
log "Test 5: Human mode pause after round 1"
STATUS=$(api -X POST -H "Content-Type: application/json" -H "Accept: application/json" \
    -d "{
        \"rounds\": 3,
        \"stop_on_convergence\": false,
        \"integration_mode\": \"human\",
        \"provider_params\": {\"max_tokens\": 800}
    }" \
    "${WORKER_URL}/auto/${WF_HUMAN}?csvkey=${CSVKEY}")
assert_status "Human mode auto-run → 200" "200" "$STATUS"
assert_json "stopped_reason=awaiting_integration" "d['data']['stopped_reason']" "awaiting_integration"
assert_json "rounds_completed=1" "d['data']['rounds_completed']" "1"
assert_json "integration_mode=human" "d['data']['integration_mode']" "human"
assert_json "has next_round" "'next_round' in d['data']" "True"
assert_json "has hint" "d.get('hint','') != ''" "True"

log ""

# Conflict: starting another auto-run while one is pending
log "Test 6: Conflict on second human auto-run"
STATUS=$(api -X POST -H "Content-Type: application/json" -H "Accept: application/json" \
    -d '{"rounds": 1, "integration_mode": "human"}' \
    "${WORKER_URL}/auto/${WF_HUMAN}?csvkey=${CSVKEY}")
assert_status "Second human auto-run → 409" "409" "$STATUS"
assert_json "Conflict code" "d['code']" "conflict"

log ""

# Update document, then resume
log "Test 7: Human resume after document update"

STATUS=$(api -X PUT -H "Content-Type: text/markdown" \
    -d "# API Spec v2 (human-updated)

## Endpoints
GET /items returns paginated items with cursor.
POST /items creates an item with validation.
DELETE /items/:id deletes an item.

## Auth
Bearer token authentication required.

## Errors
Returns structured JSON errors with codes." \
    "${WORKER_URL}/documents/${WF_HUMAN}/spec?csvkey=${CSVKEY}")
assert_status "Update spec document" "200" "$STATUS"

STATUS=$(api -X POST -H "Accept: application/json" \
    "${WORKER_URL}/auto/${WF_HUMAN}/resume?csvkey=${CSVKEY}")
assert_status "Resume auto-run → 200" "200" "$STATUS"
assert_json "stopped_reason=awaiting_integration" "d['data']['stopped_reason']" "awaiting_integration"
assert_json "rounds_completed=2" "d['data']['rounds_completed']" "2"

log ""

# Resume again without updating docs (still valid)
log "Test 8: Second resume (final round, should complete)"
STATUS=$(api -X POST -H "Accept: application/json" \
    "${WORKER_URL}/auto/${WF_HUMAN}/resume?csvkey=${CSVKEY}")
assert_status "Final resume → 200" "200" "$STATUS"
assert_json "stopped_reason=completed" "d['data']['stopped_reason']" "completed"
assert_json "rounds_completed=3" "d['data']['rounds_completed']" "3"
assert_json "integration_mode=human" "d['data']['integration_mode']" "human"

log ""

# State should be cleaned up
log "Test 9: State cleanup after completion"
STATUS=$(api -X POST -H "Accept: application/json" \
    "${WORKER_URL}/auto/${WF_HUMAN}/resume?csvkey=${CSVKEY}")
assert_status "Resume after completion → 404" "404" "$STATUS"

log ""

# ============================================================
# Summary
# ============================================================

log "=== Results: $PASS passed, $FAIL failed (out of $TOTAL) ==="
[ "$FAIL" -eq 0 ] && log "All checks passed!" || log "Some checks failed."
exit "$FAIL"
