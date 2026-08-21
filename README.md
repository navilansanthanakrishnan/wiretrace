# wiretrace

Watch an application, learn its API, hand it to an agent.

wiretrace observes the HTTP traffic a real app makes while it is used, works out
what API is behind that traffic, and turns the result into something callable —
an OpenAPI document, live replay, or a standalone MCP server. No documentation,
no reverse engineering, no scraping. Just use the app, then stop.

```
  use the app  ─────►  capture  ─────►  infer  ─────►  export
  (Chrome or                exchanges     endpoints      openapi.json
   a native app)            as JSON       + schemas      server.py (MCP)
                            lines         + auth         call it live
```

## Two ways in

Already have a HAR from your browser's DevTools? Skip the capture entirely —
no proxy, no certificate, nothing installed in your keychain:

```bash
wiretrace import ~/Downloads/app.har
wiretrace export ./app-api
```

Otherwise let wiretrace do the capturing.

## The loop

```bash
wiretrace start https://hn.algolia.com/?query=rust   # a Chrome opens
#   ... click around ...
wiretrace stop
# s1787284229: 35 requests -> 7 endpoints
# post_indexes_item_dev_query: POST https://uj5wyc0l7x-dsn.algolia.net/1/indexes/Item_dev/query (1x)

wiretrace show post_indexes_item_dev_query           # params, body schema, auth
wiretrace call post_indexes_item_dev_query --body '{"query":"zig comptime"}'
wiretrace export ./hn-api                            # openapi.json + a runnable MCP server
```

An agent does the same thing through the `wiretrace` MCP server. Same operations,
same session directory, different names:

| CLI | MCP tool |
|---|---|
| `wiretrace start` | `start_capture` |
| `wiretrace stop` | `stop_capture` |
| `wiretrace show` | `list_endpoints` / `describe_endpoint` |
| `wiretrace call` | `call_endpoint` |
| `wiretrace export` | `export_mcp` |
| `wiretrace import` | `import_har` |
| `wiretrace verify` | `verify_session` |
| `wiretrace sessions` / `wiretrace rm` | `list_sessions` / `forget_session` |
| `wiretrace ca` | `ca_path` |

wiretrace only watches. Clicking, typing and navigating are a separate job, done
by [open-computer-use](https://github.com/NavilanSanthanakrishnan/open-computer-use)
and its `computer-use` skill. Keeping the two apart is why wiretrace has no idea
what a button is: it sees requests, nothing else.

## Using it from a coding agent

`skills/wiretrace/` is a portable agent skill: copy it wherever your agent loads
skills from (`~/.claude/skills/` for Claude Code) and register `wiretrace-mcp`
as an MCP server. After that the user says "turn this portal into an API" and
the agent knows the whole procedure — which capture mode fits, how many samples
inference needs, how to read a dependency edge, and what a 401 actually means.

## How it compares

The closest tool is [mitmproxy2swagger](https://github.com/alufers/mitmproxy2swagger),
which turns a mitmproxy or HAR capture into an OpenAPI document. wiretrace reads
the same HAR files, so anything you can do there you can do here.

| | mitmproxy2swagger | wiretrace |
|---|---|---|
| HAR input | yes | yes |
| Captures traffic itself | no — run mitmproxy separately | yes, proxy or browser |
| Workflow | two passes, editing `ignore:` lines in YAML by hand | one command |
| Output | OpenAPI | OpenAPI, plus a runnable MCP server |
| Call an endpoint back | no | yes, with the captured session |
| Credentials | warns they may end up in the schema | moved to a 0600 store, kept out of the schema |
| Which click caused which request | — | recorded in browser captures |
| Call dependencies between endpoints | — | recovered by dataflow analysis |
| Typed tool schemas for agents | — | generated from inferred body schemas |

The difference in intent: mitmproxy2swagger documents an API, wiretrace makes one
usable. If you want a spec to read, either will do. If you want something an
agent can call, you need the credential handling and the replay path.

## How it is built

Two layers, split along a hard line: **Rust handles bytes, Python handles
meaning.**

| | |
|---|---|
| `capture/` (Rust, ~600 lines) | Everything that has to touch TLS, sockets and a browser. Emits one JSON line per HTTP exchange on stdout and nothing else. |
| `wiretrace/` (Python, ~700 lines) | Everything that has to be understood, changed and extended: inference, replay, code generation, the agent surface. |

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
| `api.py` | inference: group requests into endpoints, template out ids, merge schemas, spot auth |
| `dataflow.py` | which call feeds which — the values one response produced and a later request consumed |
| `har.py` | read a DevTools HAR as if we had captured it |
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

### Dependencies between calls

A request usually carries values it did not invent — a channel id from a guild
listing, a CSRF token from a page load. Replaying such a call in isolation works
until the value goes stale, and then it fails looking like broken auth.

`dataflow.py` indexes every scalar a response produced and checks every later
request against that index. A hit is an edge:

```
needs first: channel_id <- get_guilds.guilds[].id
```

It is an observation, not a guess: the value either appeared in an earlier
response or it did not. Edges land in `describe_endpoint`, the generated tool's
docstring, and `api.json`.

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

Needs macOS, a Rust toolchain, Python 3.11+, and Google Chrome for browser
captures. [uv](https://docs.astral.sh/uv/) is used below but a plain venv works.

```bash
git clone https://github.com/NavilanSanthanakrishnan/wiretrace
cd wiretrace
cargo build --release --manifest-path capture/Cargo.toml
uv venv && uv pip install -e .
source .venv/bin/activate
wiretrace --help
```

Set `WIRETRACE_HOME` to move everything wiretrace stores (sessions, the CA,
browser profiles) somewhere other than `~/.wiretrace`.

To give an agent the tools, register `wiretrace-mcp` with any MCP client. It
speaks stdio:

```json
{"mcpServers": {"wiretrace": {"command": "wiretrace-mcp"}}}
```

For native apps there are two ways to make the proxy's certificate acceptable.
Trust it system-wide once, which prompts for your password:

```bash
wiretrace trust
```

Or leave the keychain alone and point individual clients at the certificate —
`wiretrace ca` prints its path, creating it if needed:

```bash
curl -x http://127.0.0.1:8787 --cacert "$(wiretrace ca)" https://api.github.com/repositories/1300192
```

## Native apps

```bash
wiretrace start discord.com --mode proxy   # points the macOS system proxy here
#   ... use Discord ...
wiretrace stop
```

`--no-system-proxy` leaves your network settings alone and expects you to route
traffic yourself:

```bash
wiretrace start api.github.com --mode proxy --no-system-proxy
curl -x http://127.0.0.1:8787 --cacert "$(wiretrace ca)" https://api.github.com/search/repositories?q=rust
wiretrace stop
```

That is the unattended path, and how the test suite drives a capture. `start`
does not return until the capture is actually listening, so the next command
cannot race it.

`export` writes one OpenAPI document per host — `openapi.json` when a capture
covers a single host, `openapi-<host>.json` each when it spans several, since
`servers` is a document-level list and a mixed spec silently resolves every path
against the first host.

## On disk

`~/.wiretrace/sessions/<id>/`

```
session.json       what was captured and how
events.jsonl       raw exchanges, written live by the capture binary   0600
api.json           the inferred API
credentials.json   auth material                                      0600
capture.log        the capture binary's stderr, when something is wrong
```

The session directory is 0700 and the raw capture is 0600, because
`events.jsonl` is the least redacted thing here — full cookies, full tokens, and
the bodies of logged-in pages. `wiretrace rm <id>` deletes a capture and its
browser profile; they hold real credentials, so do not let them pile up.

## Limits

These are real, not temporary caveats hiding a bug:

- macOS only (system proxy and keychain trust are `networksetup` / `security`).
- One tab per browser capture. The capture attaches to a single page target, so
  a tab opened during the session is not recorded. Drive the tab that is already
  open.
- Captures meant to be driven need a visible window, because the driver is
  open-computer-use. `--headless` only suits a capture that needs nothing beyond
  loading the target URL.
- Request-to-click attribution exists only in `browser` mode. Proxy captures
  have no UI to attribute to.
- Certificate-pinned apps will not decrypt through the proxy. Nothing here
  defeats pinning.
- `browser` mode is Chromium only.
- Inference describes what was *observed*. An endpoint's optional fields are
  invisible until something sends them.

## License

MIT. See `LICENSE`.

## Tests

```bash
pytest tests
```

The suite runs a real capture — proxy, fixture API, inference, export, and it
compiles the generated MCP server.
