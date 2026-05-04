#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

pick_port() {
  local start="${1}"
  local end="${2}"
  local candidate
  for candidate in $(seq "$start" "$end"); do
    if ! lsof -nP -iTCP:"$candidate" -sTCP:LISTEN >/dev/null 2>&1; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done

  echo "no free port found in range ${start}-${end}" >&2
  return 1
}

FIXTURE_PORT="${FIXTURE_PORT:-$(pick_port 8030 8045)}"
WORKFLOW_PORT="${WORKFLOW_PORT:-$(pick_port 4336 4355)}"
FIXTURE_PID=""
SERVER_PID=""
TMP_DIR="$(mktemp -d -t agent-mcp-b-workflow-auth-e2e.XXXXXX)"
PROFILE_DIR="${TMP_DIR}/profile"

cleanup() {
  if [[ -n "${SERVER_PID}" ]] && kill -0 "${SERVER_PID}" >/dev/null 2>&1; then
    kill "${SERVER_PID}" >/dev/null 2>&1 || true
    wait "${SERVER_PID}" >/dev/null 2>&1 || true
  fi
  if [[ -n "${FIXTURE_PID}" ]] && kill -0 "${FIXTURE_PID}" >/dev/null 2>&1; then
    kill "${FIXTURE_PID}" >/dev/null 2>&1 || true
    wait "${FIXTURE_PID}" >/dev/null 2>&1 || true
  fi
  rm -rf "${TMP_DIR}"
}

fail() {
  echo "workflow auth e2e failed: $*" >&2
  exit 1
}

wait_for_http() {
  local url="${1}"
  for _ in $(seq 1 80); do
    if curl -fsS "${url}" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.25
  done
  return 1
}

trap cleanup EXIT

echo "building binary"
cargo build >/dev/null

echo "starting workflow fixture on http://127.0.0.1:${FIXTURE_PORT}"
python3 scripts/workflow_fixture.py "${FIXTURE_PORT}" >/dev/null 2>&1 &
FIXTURE_PID="$!"
wait_for_http "http://127.0.0.1:${FIXTURE_PORT}/auth" || fail "fixture auth page did not start"

echo "starting workflow server on http://127.0.0.1:${WORKFLOW_PORT}"
target/debug/agent-mcp-b workflow serve --listen "127.0.0.1:${WORKFLOW_PORT}" >/dev/null 2>&1 &
SERVER_PID="$!"
wait_for_http "http://127.0.0.1:${WORKFLOW_PORT}/api/status" || fail "workflow server did not start"

echo "beginning authenticated browser-deep recording"
BEGIN_OUTPUT="$(target/debug/agent-mcp-b workflow begin \
  --server "127.0.0.1:${WORKFLOW_PORT}" \
  --mode browser_deep \
  --open "http://127.0.0.1:${FIXTURE_PORT}/auth" \
  --user-data-dir "${PROFILE_DIR}" \
  --host-contains "127.0.0.1" \
  --url-contains "/auth/,/api/" \
  --name workflow-auth-e2e)"

SESSION_ID="$(printf '%s' "${BEGIN_OUTPUT}" | python3 -c 'import json,sys; print(json.loads(sys.stdin.read())["id"])')"
[[ -n "${SESSION_ID}" ]] || fail "did not receive a workflow session id"

sleep 5

echo "stopping recording"
STOP_OUTPUT="$(target/debug/agent-mcp-b workflow stop --server "127.0.0.1:${WORKFLOW_PORT}")"
STOP_STATUS="$(printf '%s' "${STOP_OUTPUT}" | python3 -c 'import json,sys; print(json.loads(sys.stdin.read())["status"])')"
[[ "${STOP_STATUS}" == "ready" ]] || fail "workflow session did not reach ready state"

SESSION_JSON="$(curl -fsS "http://127.0.0.1:${WORKFLOW_PORT}/api/sessions/${SESSION_ID}")"
printf '%s' "${SESSION_JSON}" | python3 -c '
import json, sys
payload = json.load(sys.stdin)
context = payload.get("context_map") or {}
ops = context.get("operations") or []
auth_signals = set(context.get("auth_signals") or [])
paths = {op.get("path") for op in ops}
required_paths = {"127.0.0.1/auth/login", "127.0.0.1/api/private"}
missing = required_paths - paths
if missing:
    raise SystemExit(f"missing auth operations: {sorted(missing)}")
required_auth = {"authorization (1)", "set-cookie (1)", "cookie (1)"}
if not required_auth.issubset(auth_signals):
    raise SystemExit(f"missing auth signals: {sorted(required_auth - auth_signals)}")
' || fail "authenticated context map validation failed"

echo "workflow auth e2e passed for session ${SESSION_ID}"
