#!/usr/bin/env bash
set -euo pipefail

# Post-deploy verification script for APRP
# Usage: ./scripts/verify-deploy.sh <worker-url> <csvkey>
# Example: ./scripts/verify-deploy.sh https://rusty-convergence.workers.dev my-secret-key

WORKER_URL="${1:?Usage: $0 <worker-url> <csvkey>}"
CSVKEY="${2:?Usage: $0 <worker-url> <csvkey>}"

pass=0
fail=0

check() {
    local desc="$1" expected_status="$2" url="$3"
    shift 3
    local status
    status=$(curl -s -o /dev/null -w "%{http_code}" "$@" "$url")
    if [ "$status" = "$expected_status" ]; then
        echo "  PASS: $desc (HTTP $status)"
        pass=$((pass + 1))
    else
        echo "  FAIL: $desc (expected $expected_status, got $status)"
        fail=$((fail + 1))
    fi
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

echo "4. Method validation"
check "POST /health returns 405" 405 "$WORKER_URL/health" -X POST

echo "5. Not found"
check "Unknown path returns 404" 404 "$WORKER_URL/nonexistent?csvkey=$CSVKEY"

echo "6. Document upload + retrieve"
DOC_URL="$WORKER_URL/documents/verify-test/readme?csvkey=$CSVKEY"
check "PUT small doc returns 200" 200 "$DOC_URL" \
    -X PUT -H "Content-Type: text/markdown" -d "# Test README for verification"

check "GET doc returns 200" 200 "$DOC_URL"

echo "7. Workflow CRUD"
WF_URL="$WORKER_URL/workflows?csvkey=$CSVKEY"
check "POST workflow returns 200" 200 "$WF_URL" \
    -X POST -H "Content-Type: application/json" \
    -d '{"name":"verify-test","provider":"openai","model":"o3","documents":{"readme":"readme"},"template":"{{readme}}"}'

check "GET workflow returns 200" 200 "$WORKER_URL/workflows/verify-test?csvkey=$CSVKEY"

echo "8. Cleanup"
check "DELETE workflow returns 200" 200 "$WORKER_URL/workflows/verify-test?csvkey=$CSVKEY" -X DELETE

echo
echo "=== Results: $pass passed, $fail failed ==="
[ "$fail" -eq 0 ] && echo "All checks passed!" || echo "Some checks failed."
exit "$fail"
