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

FIXTURE_PORT="${FIXTURE_PORT:-$(pick_port 8050 8065)}"
WORKFLOW_PORT="${WORKFLOW_PORT:-$(pick_port 4356 4370)}"
FIXTURE_PID=""
SERVER_PID=""
TMP_DIR="$(mktemp -d -t agent-mcp-b-workflow-desktop-e2e.XXXXXX)"

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
  echo "workflow desktop e2e failed: $*" >&2
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

proxy_disabled() {
  local value
  value="$(networksetup -getwebproxy Wi-Fi)"
  [[ "${value}" == *"Enabled: No"* ]] || return 1
  value="$(networksetup -getsecurewebproxy Wi-Fi)"
  [[ "${value}" == *"Enabled: No"* ]]
}

trap cleanup EXIT

echo "building binary"
cargo build >/dev/null

echo "starting workflow fixture on http://127.0.0.1:${FIXTURE_PORT}"
python3 scripts/workflow_fixture.py "${FIXTURE_PORT}" >/dev/null 2>&1 &
FIXTURE_PID="$!"
wait_for_http "http://127.0.0.1:${FIXTURE_PORT}/" || fail "fixture did not start"

echo "starting workflow server on http://127.0.0.1:${WORKFLOW_PORT}"
target/debug/agent-mcp-b workflow serve --listen "127.0.0.1:${WORKFLOW_PORT}" >/dev/null 2>&1 &
SERVER_PID="$!"
wait_for_http "http://127.0.0.1:${WORKFLOW_PORT}/api/status" || fail "workflow server did not start"

echo "beginning desktop recording"
BEGIN_OUTPUT="$(target/debug/agent-mcp-b workflow begin \
  --server "127.0.0.1:${WORKFLOW_PORT}" \
  --mode desktop \
  --host-contains "127.0.0.1" \
  --url-contains "/api/" \
  --name workflow-desktop-e2e)"

SESSION_ID="$(printf '%s' "${BEGIN_OUTPUT}" | python3 -c 'import json,sys; print(json.loads(sys.stdin.read())["id"])')"
RECORDER_ENDPOINT="$(printf '%s' "${BEGIN_OUTPUT}" | python3 -c 'import json,sys; print(json.loads(sys.stdin.read()).get("recorder_endpoint") or "")')"
[[ -n "${SESSION_ID}" ]] || fail "did not receive a workflow session id"
[[ -n "${RECORDER_ENDPOINT}" ]] || fail "desktop session did not expose recorder endpoint"

curl -fsS --proxy "${RECORDER_ENDPOINT}" "http://127.0.0.1:${FIXTURE_PORT}/api/config" >/dev/null
curl -fsS --proxy "${RECORDER_ENDPOINT}" \
  -H 'content-type: application/json' \
  -d '{"action":"ship","target":"message","count":1}' \
  "http://127.0.0.1:${FIXTURE_PORT}/api/submit" >/dev/null
sleep 1

echo "stopping recording"
STOP_OUTPUT="$(target/debug/agent-mcp-b workflow stop --server "127.0.0.1:${WORKFLOW_PORT}")"
STOP_STATUS="$(printf '%s' "${STOP_OUTPUT}" | python3 -c 'import json,sys; print(json.loads(sys.stdin.read())["status"])')"
[[ "${STOP_STATUS}" == "ready" ]] || fail "workflow session did not reach ready state"

proxy_disabled || fail "system proxy settings were not restored"

SESSION_JSON="$(curl -fsS "http://127.0.0.1:${WORKFLOW_PORT}/api/sessions/${SESSION_ID}")"
printf '%s' "${SESSION_JSON}" | python3 -c '
import json, sys
payload = json.load(sys.stdin)
context = payload.get("context_map") or {}
ops = context.get("operations") or []
paths = {op.get("path") for op in ops}
required_paths = {"127.0.0.1/api/config", "127.0.0.1/api/submit"}
missing = required_paths - paths
if missing:
    raise SystemExit(f"missing desktop operations: {sorted(missing)}")
' || fail "desktop context map validation failed"

echo "workflow desktop e2e passed for session ${SESSION_ID}"
