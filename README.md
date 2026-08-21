# reqtrace

Watch an application, learn its API, hand it to an agent.

reqtrace observes the HTTP traffic a real app makes while it is used, works out
what API is behind that traffic, and turns the result into something callable —
an OpenAPI document, live replay, or a standalone MCP server. No documentation,
no reverse engineering, no scraping. Just use the app, then stop.

```
  use the app  ─────►  capture  ─────►  infer  ─────►  export
  (Chrome or                exchanges     endpoints      openapi.json
   a native app)            as JSON       + schemas      server.py (MCP)
                            lines         + auth         call it live
```

## The loop

```bash
reqtrace start https://hn.algolia.com/?query=rust   # a Chrome opens
#   ... click around ...
reqtrace stop
# s1787284229: 35 requests -> 7 endpoints
# post_indexes_item_dev_query: POST https://uj5wyc0l7x-dsn.algolia.net/1/indexes/Item_dev/query (1x)

reqtrace show post_indexes_item_dev_query           # params, body schema, auth
reqtrace call post_indexes_item_dev_query --body '{"query":"claude code"}'
reqtrace export ./hn-api                            # openapi.json + a runnable MCP server
```

Add `--headless` to `start` and drive the page yourself with `reqtrace open`
and `reqtrace eval` — that is the unattended path, and it needs no window.

An agent does the same thing through the `reqtrace` MCP server. Same operations,
same session directory, different names:

| CLI | MCP tool |
|---|---|
| `reqtrace start` | `start_capture` |
| `reqtrace open <url>` / `reqtrace eval <js>` | `navigate` / `evaluate` |
| `reqtrace stop` | `stop_capture` |
| `reqtrace show` | `list_endpoints` / `describe_endpoint` |
| `reqtrace call` | `call_endpoint` |
| `reqtrace export` | `export_mcp` |
| `reqtrace sessions` / `reqtrace rm` | `list_sessions` / `forget_session` |
| `reqtrace ca` | `ca_path` |

## How it is built

Two layers, split along a hard line: **Rust handles bytes, Python handles
meaning.**

| | |
|---|---|
| `capture/` (Rust, ~600 lines) | Everything that has to touch TLS, sockets and a browser. Emits one JSON line per HTTP exchange on stdout and nothing else. |
| `reqtrace/` (Python, ~700 lines) | Everything that has to be understood, changed and extended: inference, replay, code generation, the agent surface. |

The whole contract between them is one JSON object per line
(`capture/src/event.rs`). That is why the Rust side has no config, no server, no
state, and no model calls — it cannot grow.

### The capture layer

Two ways to see traffic, one output format:

- **`browser`** launches a managed Chrome, enables the DevTools `Network`
  domain, and injects a page script that reports clicks. Requests are attributed
  to the UI action that caused them, so a capture reads as *this button calls
  this endpoint*.
- **`proxy`** terminates TLS with a leaf certificate from a local CA, forwards
  upstream, and records the plaintext. This is how native desktop apps are
  captured — they reach it through the macOS system proxy.

Only the hosts you name are intercepted. Everything else is blind-tunnelled:
never decrypted, never recorded, and reaching its real certificate untouched. So
turning on the system proxy exposes the hosts you asked for and nothing else.

### The pipeline

| file | job |
|---|---|
| `events.py` | the captured exchange, and the one predicate that separates API calls from page furniture |
| `cdp.py` | a tiny client for *driving* the captured tab — navigate and evaluate |
| `api.py` | inference: group requests into endpoints, template out ids, merge schemas, spot auth headers |
| `session.py` | start / stop a capture, own the session directory |
| `call.py` | replay an endpoint, filling in whatever the caller did not specify |
| `export.py` | OpenAPI 3.1, and a generated MCP server |
| `server.py` | the MCP server an agent drives |
| `cli.py` | the same operations for a human |

Inference is deterministic — no model is involved anywhere. `/channels/8412/messages`
and `/channels/9917/messages` become one endpoint
`POST /channels/{channel_id}/messages` because the varying segment looks like an
identifier, not because something guessed.

That test is deliberately conservative: a segment has to look machine-generated.
`/repos/octocat/Hello-World` stays literal, so two repositories give you two
endpoints. Under-merging costs you a duplicate; over-merging fuses two different
calls into one and breaks replay, so the bias goes this way on purpose.

### Credentials

Anything whose name looks like a credential — `authorization`, `cookie`, and any
header *or query parameter* containing `key`, `token`, `secret`, `auth`,
`session`, `sig` — has its value moved into `credentials.json` at mode 0600.
Query strings count: plenty of services authenticate with `?api_key=`, and a
scheme that only guarded headers would leak those into every export.

The API description keeps names, never values — in endpoint fields, example
URLs, and example bodies alike. Replay attaches the real values.
**The agent can use the app's session without ever seeing the token.**

## Install

```bash
cargo build --release --manifest-path capture/Cargo.toml
uv venv && uv pip install -e .
```

Set `REQTRACE_HOME` to move everything reqtrace stores (sessions, the CA,
browser profiles) somewhere other than `~/.reqtrace`.

For native apps there are two ways to make the proxy's certificate acceptable.
Trust it system-wide once, which prompts for your password:

```bash
reqtrace trust
```

Or leave the keychain alone and point individual clients at the certificate —
`reqtrace ca` prints its path, creating it if needed:

```bash
curl -x http://127.0.0.1:8787 --cacert "$(reqtrace ca)" https://api.github.com/repositories/1300192
```

## Native apps

```bash
reqtrace start discord.com --mode proxy   # points the macOS system proxy here
#   ... use Discord ...
reqtrace stop
```

`--no-system-proxy` leaves your network settings alone and expects you to route
traffic yourself:

```bash
reqtrace start api.github.com --mode proxy --no-system-proxy
curl -x http://127.0.0.1:8787 --cacert "$(reqtrace ca)" https://api.github.com/search/repositories?q=rust
reqtrace stop
```

That is the unattended path, and how the test suite drives a capture. `start`
does not return until the capture is actually listening, so the next command
cannot race it.

`export` writes one OpenAPI document per host — `openapi.json` when a capture
covers a single host, `openapi-<host>.json` each when it spans several, since
`servers` is a document-level list and a mixed spec silently resolves every path
against the first host.

## On disk

`~/.reqtrace/sessions/<id>/`

```
session.json       what was captured and how
events.jsonl       raw exchanges, written live by the capture binary   0600
api.json           the inferred API
credentials.json   auth material                                      0600
capture.log        the capture binary's stderr, when something is wrong
```

The session directory is 0700 and the raw capture is 0600, because
`events.jsonl` is the least redacted thing here — full cookies, full tokens, and
the bodies of logged-in pages. `reqtrace rm <id>` deletes a capture and its
browser profile; they hold real credentials, so do not let them pile up.

## Limits

These are real, not temporary caveats hiding a bug:

- macOS only (system proxy and keychain trust are `networksetup` / `security`).
- One tab per browser capture. The capture attaches to a single page target, so
  drive it with `navigate`/`evaluate` — a tab you open some other way is not
  recorded.
- Request-to-click attribution exists only in `browser` mode. Proxy captures
  have no UI to attribute to.
- Certificate-pinned apps will not decrypt through the proxy. Nothing here
  defeats pinning.
- `browser` mode is Chromium only.
- Inference describes what was *observed*. An endpoint's optional fields are
  invisible until something sends them.

## Tests

```bash
pytest tests
```

The suite runs a real capture — proxy, fixture API, inference, export, and it
compiles the generated MCP server.
