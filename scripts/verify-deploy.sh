#!/usr/bin/env bash
set -euo pipefail

# Post-deploy verification script for APRP
# Usage: ./scripts/verify-deploy.sh <worker-url> <csvkey>
# Example: ./scripts/verify-deploy.sh https://rusty-convergence.workers.dev my-secret-key

WORKER_URL="${1:?Usage: $0 <worker-url> <csvkey>}"
CSVKEY="${2:?Usage: $0 <worker-url> <csvkey>}"

pass=0
fail=0

record_pass() {
    local desc="$1"
    echo "  PASS: $desc"
    pass=$((pass + 1))
}

record_fail() {
    local desc="$1"
    echo "  FAIL: $desc"
    fail=$((fail + 1))
}

check() {
    local desc="$1" expected_status="$2" url="$3"
    shift 3
    local status
    status=$(curl -s -o /dev/null -w "%{http_code}" "$@" "$url")
    if [ "$status" = "$expected_status" ]; then
        record_pass "$desc (HTTP $status)"
    else
        record_fail "$desc (expected $expected_status, got $status)"
    fi
}

check_secret_flag() {
    local body="$1" secret_name="$2"
    local compact_body="${body//[[:space:]]/}"
    case "$compact_body" in
        *"\"$secret_name\":true"*)
            record_pass "$secret_name is configured"
            ;;
        *"\"$secret_name\":false"*)
            record_fail "$secret_name is missing from Worker secrets"
            ;;
        *)
            record_fail "$secret_name diagnostic flag was not present"
            ;;
    esac
}

check_health_diagnostics() {
    local response body status
    response=$(curl -sS -w $'\n%{http_code}' "$WORKER_URL/health?csvkey=$CSVKEY")
    status="${response##*$'\n'}"
    body="${response%$'\n'*}"

    if [ "$status" = "200" ]; then
        record_pass "GET /health with csvkey returns 200 (HTTP $status)"
    else
        record_fail "GET /health with csvkey returns 200 (expected 200, got $status)"
        return
    fi

    check_secret_flag "$body" "OPENAI_API_KEY"
    check_secret_flag "$body" "ANTHROPIC_API_KEY"
}

echo "=== APRP Deploy Verification ==="
echo "Target: $WORKER_URL"
echo

echo "1. Health check (unauthenticated)"
check "GET /health returns 200" 200 "$WORKER_URL/health"

echo "2. Auth rejection"
check "Wrong csvkey returns 401" 401 "$WORKER_URL/workflows?csvkey=wrong-key"

echo "3. Auth success"
check "Correct csvkey returns 200" 200 "$WORKER_URL/workflows?csvkey=$CSVKEY"

echo "4. Provider secret preflight"
check_health_diagnostics

echo "5. Method validation"
check "POST /health returns 405" 405 "$WORKER_URL/health" -X POST

echo "6. Not found"
check "Unknown path returns 404" 404 "$WORKER_URL/nonexistent?csvkey=$CSVKEY"

echo "7. Document upload + retrieve"
DOC_URL="$WORKER_URL/documents/verify-test/readme?csvkey=$CSVKEY"
check "PUT small doc returns 200" 200 "$DOC_URL" \
    -X PUT -H "Content-Type: text/markdown" -d "# Test README for verification"

check "GET doc returns 200" 200 "$DOC_URL"

echo "8. Workflow CRUD"
WF_URL="$WORKER_URL/workflows?csvkey=$CSVKEY"
check "POST workflow returns 200" 200 "$WF_URL" \
    -X POST -H "Content-Type: application/json" \
    -d '{"name":"verify-test","provider":"openai","model":"o3","documents":{"readme":"readme"},"template":"{{readme}}"}'

check "GET workflow returns 200" 200 "$WORKER_URL/workflows/verify-test?csvkey=$CSVKEY"

echo "9. Cleanup"
check "DELETE workflow returns 200" 200 "$WORKER_URL/workflows/verify-test?csvkey=$CSVKEY" -X DELETE

echo
echo "=== Results: $pass passed, $fail failed ==="
[ "$fail" -eq 0 ] && echo "All checks passed!" || echo "Some checks failed."
exit "$fail"
