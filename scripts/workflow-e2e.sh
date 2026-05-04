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

FIXTURE_PORT="${FIXTURE_PORT:-$(pick_port 8012 8025)}"
WORKFLOW_PORT="${WORKFLOW_PORT:-$(pick_port 4317 4335)}"
FIXTURE_PID=""
SERVER_PID=""
TMP_DIR="$(mktemp -d -t agent-mcp-b-workflow-e2e.XXXXXX)"
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
  echo "workflow e2e failed: $*" >&2
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
wait_for_http "http://127.0.0.1:${FIXTURE_PORT}/" || fail "fixture did not start"

echo "starting workflow server on http://127.0.0.1:${WORKFLOW_PORT}"
target/debug/agent-mcp-b workflow serve --listen "127.0.0.1:${WORKFLOW_PORT}" >/dev/null 2>&1 &
SERVER_PID="$!"
wait_for_http "http://127.0.0.1:${WORKFLOW_PORT}/api/status" || fail "workflow server did not start"

echo "checking localhost UI"
curl -fsS "http://127.0.0.1:${WORKFLOW_PORT}/" | grep -F "Workflow Studio" >/dev/null \
  || fail "localhost UI did not render expected title"

echo "beginning browser-deep recording"
BEGIN_OUTPUT="$(target/debug/agent-mcp-b workflow begin \
  --server "127.0.0.1:${WORKFLOW_PORT}" \
  --mode browser_deep \
  --open "http://127.0.0.1:${FIXTURE_PORT}/" \
  --user-data-dir "${PROFILE_DIR}" \
  --name workflow-e2e)"

SESSION_ID="$(printf '%s' "${BEGIN_OUTPUT}" | python3 -c 'import json,sys; print(json.loads(sys.stdin.read())["id"])')"
[[ -n "${SESSION_ID}" ]] || fail "did not receive a workflow session id"

sleep 4

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
summary = context.get("summary", "")
if not ops:
    raise SystemExit("no operations in context map")
paths = {op.get("path") for op in ops}
if "127.0.0.1/api/config" not in paths:
    raise SystemExit(f"missing GET config operation: {paths}")
if "127.0.0.1/api/submit" not in paths:
    raise SystemExit(f"missing POST submit operation: {paths}")
if "Captured" not in summary:
    raise SystemExit(f"unexpected context summary: {summary}")
' || fail "context map validation failed"

echo "generating automation artifact"
ASK_OUTPUT="$(target/debug/agent-mcp-b workflow ask \
  --server "127.0.0.1:${WORKFLOW_PORT}" \
  --session-id "${SESSION_ID}" \
  "Build an automation that replays the submit operation with a configurable payload.")"

printf '%s' "${ASK_OUTPUT}" | python3 -c '
import json, sys
payload = json.load(sys.stdin)
summary = payload.get("summary", "")
files = payload.get("generated_files") or []
if not summary:
    raise SystemExit("automation summary missing")
if not files:
    raise SystemExit("automation files missing")
paths = {item.get("path") for item in files}
if "automation-plan.md" not in paths:
    raise SystemExit(f"unexpected generated file set: {paths}")
' || fail "automation generation validation failed"

echo "workflow e2e passed for session ${SESSION_ID}"
