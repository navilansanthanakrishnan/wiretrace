#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

pick_port() {
  local candidate
  for candidate in $(seq 8850 8865); do
    if ! lsof -nP -iTCP:"$candidate" -sTCP:LISTEN >/dev/null 2>&1; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done

  echo "no free eval port found in 8850-8865" >&2
  return 1
}

PORT="${1:-$(pick_port)}"
PROXY_URL="http://127.0.0.1:${PORT}"
LOG_FILE="$(mktemp -t agent-mcp-b-smoke-log.XXXXXX)"
PID=""

cleanup() {
  if [[ -n "${PID}" ]] && kill -0 "${PID}" >/dev/null 2>&1; then
    kill "${PID}" >/dev/null 2>&1 || true
    wait "${PID}" >/dev/null 2>&1 || true
  fi
  rm -f "${LOG_FILE}"
}

fail() {
  echo "smoke eval failed: $*" >&2
  echo "--- proxy output ---" >&2
  cat "${LOG_FILE}" >&2 || true
  exit 1
}

trap cleanup EXIT

echo "building binary"
cargo build >/dev/null

echo "starting proxy on ${PROXY_URL}"
target/debug/agent-mcp-b proxy --listen "127.0.0.1:${PORT}" --output focused >"${LOG_FILE}" 2>&1 &
PID="$!"

for _ in $(seq 1 50); do
  if lsof -nP -iTCP:"${PORT}" -sTCP:LISTEN >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done

if ! lsof -nP -iTCP:"${PORT}" -sTCP:LISTEN >/dev/null 2>&1; then
  fail "proxy did not start listening"
fi

echo "running live smoke matrix"
curl -ksS --proxy "${PROXY_URL}" https://example.com >/dev/null
curl -ksS --proxy "${PROXY_URL}" https://httpbin.org/json >/dev/null
curl -ksS --proxy "${PROXY_URL}" --compressed https://httpbin.org/gzip >/dev/null
curl -ksS --proxy "${PROXY_URL}" \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer super-secret-token' \
  -d '{"hello":"world"}' \
  https://httpbin.org/anything >/dev/null
curl -ksS --proxy "${PROXY_URL}" https://jsonplaceholder.typicode.com/posts/1 >/dev/null
curl -ksS --proxy "${PROXY_URL}" https://discord.com/api/v9/experiments >/dev/null

sleep 1

echo "validating proxy output"
grep -F "[flow] GET https://httpbin.org/json" "${LOG_FILE}" >/dev/null \
  || fail "missing httpbin json flow"
grep -F "[flow] GET https://httpbin.org/gzip" "${LOG_FILE}" >/dev/null \
  || fail "missing httpbin gzip flow"
grep -F "[flow] POST https://httpbin.org/anything" "${LOG_FILE}" >/dev/null \
  || fail "missing httpbin post flow"
grep -F "[flow] GET https://jsonplaceholder.typicode.com/posts/1" "${LOG_FILE}" >/dev/null \
  || fail "missing jsonplaceholder flow"
grep -F "[flow] GET https://discord.com/api/v9/experiments" "${LOG_FILE}" >/dev/null \
  || fail "missing discord experiments flow"
grep -F "authorization: <redacted>" "${LOG_FILE}" >/dev/null \
  || fail "authorization header was not redacted"

if grep -F "[flow] GET https://example.com/" "${LOG_FILE}" >/dev/null; then
  fail "focused mode should not emit basic HTML page loads"
fi

if grep -F "super-secret-token" "${LOG_FILE}" >/dev/null; then
  fail "sensitive authorization value leaked into output"
fi

echo "smoke eval passed"
