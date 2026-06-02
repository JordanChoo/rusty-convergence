#!/usr/bin/env bash
set -euo pipefail

# E2E Error Sweep — verifies every error code against a deployed Worker
# Requires: CSVKEY, WORKER_URL env vars
# Usage: CSVKEY=xxx WORKER_URL=https://your-worker.dev ./tests/e2e_error_sweep.sh

: "${CSVKEY:?Set CSVKEY env var}"
: "${WORKER_URL:?Set WORKER_URL env var (e.g. https://rusty-convergence.your-subdomain.workers.dev)}"

PASS=0
FAIL=0
TOTAL=0
LOGDIR="logs"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
LOGFILE="${LOGDIR}/e2e_errors_${TIMESTAMP}.log"
mkdir -p "$LOGDIR"

log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$LOGFILE"; }

check() {
    local desc="$1"
    local expected_status="$2"
    local expected_code="$3"
    local actual_status="$4"
    local actual_body="$5"
    TOTAL=$((TOTAL + 1))

    local actual_code
    actual_code=$(echo "$actual_body" | python3 -c "import json,sys; print(json.load(sys.stdin).get('code',''))" 2>/dev/null || echo "")

    if [[ "$actual_status" == "$expected_status" ]] && [[ "$actual_code" == "$expected_code" ]]; then
        log "  PASS: $desc (status=$actual_status code=$actual_code)"
        PASS=$((PASS + 1))
    else
        log "  FAIL: $desc — expected status=$expected_status code=$expected_code, got status=$actual_status code=$actual_code"
        log "    Body: ${actual_body:0:300}"
        FAIL=$((FAIL + 1))
    fi
}

log "=== E2E Error Sweep ==="
log "Worker: $WORKER_URL"
log ""

# 1. Health without csvkey → 200
log "Test 1: GET /health without csvkey"
STATUS=$(curl -s -o /tmp/e2e_body -w '%{http_code}' "${WORKER_URL}/health" 2>>"$LOGFILE")
BODY=$(cat /tmp/e2e_body)
check "GET /health no auth" "200" "ok" "$STATUS" "$BODY"

# 2. Workflows without csvkey → 401 missing_csvkey
log "Test 2: GET /workflows without csvkey"
STATUS=$(curl -s -o /tmp/e2e_body -w '%{http_code}' "${WORKER_URL}/workflows" 2>>"$LOGFILE")
BODY=$(cat /tmp/e2e_body)
check "GET /workflows no csvkey" "401" "missing_csvkey" "$STATUS" "$BODY"

# 3. Workflows with wrong csvkey → 401 unauthorized
log "Test 3: GET /workflows with wrong csvkey"
STATUS=$(curl -s -o /tmp/e2e_body -w '%{http_code}' "${WORKER_URL}/workflows?csvkey=wrongkey" 2>>"$LOGFILE")
BODY=$(cat /tmp/e2e_body)
check "GET /workflows wrong csvkey" "401" "unauthorized" "$STATUS" "$BODY"

# 4. POST to /health → 405 method_not_allowed
log "Test 4: POST /health"
STATUS=$(curl -s -o /tmp/e2e_body -w '%{http_code}' -X POST "${WORKER_URL}/health" 2>>"$LOGFILE")
BODY=$(cat /tmp/e2e_body)
check "POST /health" "405" "method_not_allowed" "$STATUS" "$BODY"

# 5. Unknown path → 404 not_found
log "Test 5: GET /nonexistent"
STATUS=$(curl -s -o /tmp/e2e_body -w '%{http_code}' "${WORKER_URL}/nonexistent?csvkey=${CSVKEY}" 2>>"$LOGFILE")
BODY=$(cat /tmp/e2e_body)
check "GET /nonexistent" "404" "not_found" "$STATUS" "$BODY"

# 6. Invalid JSON body → 400 or 500
log "Test 6: POST /workflows with invalid JSON"
STATUS=$(curl -s -o /tmp/e2e_body -w '%{http_code}' -X POST \
  -H "Content-Type: application/json" \
  -d "not json" \
  "${WORKER_URL}/workflows?csvkey=${CSVKEY}" 2>>"$LOGFILE")
BODY=$(cat /tmp/e2e_body)
# This may return 400 or 500 depending on how the body parse error is handled
if [[ "$STATUS" == "400" ]] || [[ "$STATUS" == "500" ]]; then
    log "  PASS: POST /workflows invalid JSON (status=$STATUS)"
    PASS=$((PASS + 1))
    TOTAL=$((TOTAL + 1))
else
    log "  FAIL: POST /workflows invalid JSON — expected 400 or 500, got $STATUS"
    FAIL=$((FAIL + 1))
    TOTAL=$((TOTAL + 1))
fi

# 7. GET nonexistent workflow → 404
log "Test 7: GET /workflows/nonexistent"
STATUS=$(curl -s -o /tmp/e2e_body -w '%{http_code}' "${WORKER_URL}/workflows/nonexistent-wf-12345?csvkey=${CSVKEY}" 2>>"$LOGFILE")
BODY=$(cat /tmp/e2e_body)
check "GET /workflows/nonexistent" "404" "not_found" "$STATUS" "$BODY"

# 8. GET nonexistent round → 404
log "Test 8: GET /rounds/nonexistent/1"
STATUS=$(curl -s -o /tmp/e2e_body -w '%{http_code}' "${WORKER_URL}/rounds/nonexistent-wf-12345/1?csvkey=${CSVKEY}" 2>>"$LOGFILE")
BODY=$(cat /tmp/e2e_body)
check "GET /rounds/nonexistent/1" "404" "not_found" "$STATUS" "$BODY"

log ""
log "=== SUMMARY ==="
log "Total: $TOTAL  Passed: $PASS  Failed: $FAIL"
log "Log: $LOGFILE"

if [[ $FAIL -gt 0 ]]; then
    exit 1
fi
exit 0
