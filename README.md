# agent-mcp-b

`agent-mcp-b` is a Rust-based local protocol observation runtime for turning software usage into structured network workflows.

It has three operating layers:

- HTTP(S) interception for desktop and browser traffic routed through a local proxy
- Chromium/CDP instrumentation for high-accuracy browser interaction-to-request attribution
- Workflow recording and analysis over the captured event stream, with a localhost UI and optional OpenAI-backed automation synthesis

The current build is targeted at:

- existing macOS apps that honor system proxy settings
- managed Chrome/Chromium sessions
- workflow capture, normalization, context-map generation, and automation scaffolding

It is not yet a transparent packet interceptor and it does not bypass certificate pinning.

## Architecture

The system is split into four main subsystems.

### 1. Proxy and Capture

The proxy runtime terminates outbound HTTP(S), forwards requests upstream, and captures the application-layer exchange.

- `src/proxy/runtime.rs`
  boots the local proxy and binds lifecycle/shutdown handling
- `src/proxy/authority.rs`
  persists the local CA and generates per-host leaf certificates for MITM TLS
- `src/proxy/capture.rs`
  buffers requests/responses, decodes compressed bodies, redacts or preserves sensitive material depending on flags, and renders output in `focused`, `simple`, `pretty`, or `json`
- `src/local_ca.rs`
  manages macOS CA trust installation and diagnostics

For HTTPS, the runtime establishes two TLS sessions:

- client -> local proxy
- local proxy -> origin

That gives the proxy plaintext visibility into the HTTP request/response while preserving normal upstream connectivity.

### 2. Routing Modes

Traffic reaches the runtime in three different ways.

- `attach`
  points macOS web and secure web proxies at the local listener using `networksetup`
- `chrome`
  launches a managed Chrome session with explicit proxy flags
- `browser-deep`
  launches a managed Chrome/Chromium session with the Chrome DevTools Protocol enabled, injects DOM listeners, and correlates actual page interactions to network requests

Relevant files:

- `src/attach.rs`
- `src/chrome.rs`
- `src/browser_deep.rs`
- `src/system_proxy.rs`
- `src/shutdown.rs`

### 3. Interaction Attribution

There are two interaction models in the codebase.

- proxy interaction mode (`manual` / `auto`)
  uses terminal arming or macOS global input hooks to correlate a request burst with a recent interaction window
- browser-deep attribution
  uses CDP `Runtime` + `Network` events and page-side listeners for `click`, `submit`, and `keydown`

Browser-deep mode is the most accurate path for websites and Chromium-based apps because it attributes requests inside the browser runtime instead of inferring them only from timing.

Relevant file:

- `src/interaction.rs`

### 4. Workflow Recording and Analysis

The workflow system is a server-backed orchestration layer on top of capture.

It records raw events to disk, normalizes them into operation-level events, builds a context map, then optionally sends the normalized workflow to an OpenAI Responses API backend for higher-level analysis and automation generation.

Relevant files:

- `src/workflow/server.rs`
  localhost API and browser UI
- `src/workflow/recorder.rs`
  spawns and supervises recorder child processes, normalizes their output, and builds context maps
- `src/workflow/store.rs`
  persistent workflow session storage
- `src/workflow/types.rs`
  workflow, context-map, and automation schemas
- `src/workflow/llm.rs`
  OpenAI Responses API integration plus fallback generation when no API key is configured

## Workflow Studio

Workflow Studio is the localhost control plane for record -> stop -> analyze -> automate.

When the server is running:

- `POST /api/recordings/begin`
  starts a workflow session
- `POST /api/recordings/stop`
  stops capture, normalizes events, and materializes a context map
- `GET /api/sessions`
  lists sessions
- `GET /api/sessions/{session_id}`
  returns session metadata plus the generated context map
- `POST /api/sessions/{session_id}/ask`
  sends a user prompt plus the recorded context to the automation generation path

The UI served at `/` exposes the same flow:

- begin recording
- stop and analyze
- inspect the generated context map
- ask for an automation

## OpenAI Backend

The workflow analysis path uses the OpenAI Responses API when an API key is available.

Environment variables:

- `OPENAI_API_KEY`
  required for live LLM analysis/generation
- `OPENAI_MODEL`
  optional, defaults to `gpt-5`
- `OPENAI_BASE_URL`
  optional, defaults to `https://api.openai.com/v1`

If `OPENAI_API_KEY` is not set:

- workflow recording still works
- context-map generation still works
- `workflow ask` falls back to a deterministic `automation-plan.md` generator instead of calling the API

## Commands

### Runtime Paths

```bash
cargo run -- paths
```

### CA Trust

```bash
cargo run -- ca status
cargo run -- ca trust
```

### Proxy Capture

Run the proxy directly:

```bash
cargo run -- proxy --listen 127.0.0.1:8787 --output focused
```

Attach existing macOS apps that honor the system proxy:

```bash
cargo run -- attach \
  --listen 127.0.0.1:8787 \
  --service Wi-Fi \
  --host-contains discord.com \
  --output pretty
```

Managed Chrome through the proxy:

```bash
cargo run -- chrome \
  --listen 127.0.0.1:8787 \
  --open https://discord.com \
  --host-contains discord.com \
  --url-contains /api/ \
  --output focused
```

High-accuracy browser attribution via CDP:

```bash
cargo run -- browser-deep \
  --open https://discord.com/channels/@me \
  --host-contains discord.com \
  --url-contains /api/ \
  --user-data-dir /tmp/agent-mcp-b-discord-profile \
  --output simple
```

### Workflow Studio

Start the localhost server:

```bash
cargo run -- workflow serve --listen 127.0.0.1:4317
```

Start a desktop workflow recording:

```bash
cargo run -- workflow begin \
  --server 127.0.0.1:4317 \
  --mode desktop \
  --name "discord-session"
```

Start a browser-deep workflow recording:

```bash
cargo run -- workflow begin \
  --server 127.0.0.1:4317 \
  --mode browser_deep \
  --open https://discord.com/channels/@me \
  --user-data-dir /tmp/agent-mcp-b-discord-profile \
  --name "discord-message-send"
```

Stop the active workflow:

```bash
cargo run -- workflow stop --server 127.0.0.1:4317
```

Inspect server status:

```bash
cargo run -- workflow status --server 127.0.0.1:4317
```

Ask the automation generator to synthesize an automation plan:

```bash
cargo run -- workflow ask \
  --server 127.0.0.1:4317 \
  --session-id wf-123 \
  "Build an automation that replays the message send operation with a configurable payload."
```

Then open:

- [http://127.0.0.1:4317/](http://127.0.0.1:4317/)

## Data Model

Each workflow session stores:

- `session.json`
  lifecycle metadata
- `raw-events.jsonl`
  recorder child-process event stream
- `normalized-events.json`
  normalized request/interaction records
- `context-map.json`
  summarized domains, operations, reads, writes, and interaction-to-operation edges
- `generated/`
  generated automation artifacts

The workflow context map captures:

- domains touched by the workflow
- operation signatures such as `POST discord.com/api/v9/channels/:id/messages`
- request/response summaries
- auth material signals like `authorization`, `cookie`, or `set-cookie`
- interaction edges showing which click/keydown likely triggered which operations
- optional LLM-generated analysis

## Testing and Verification

Unit and integration checks:

```bash
cargo test
cargo check
```

Proxy smoke matrix:

```bash
bash ./scripts/smoke-eval-sites.sh
```

Workflow end-to-end eval:

```bash
bash ./scripts/workflow-e2e.sh
```

The workflow e2e script verifies:

- workflow server startup
- localhost UI rendering
- browser-deep recording against a local fixture page
- raw event capture
- context-map generation
- automation generation output

## Operational Limits

Current limits are explicit:

- `desktop` workflow mode is only as broad as the system proxy path; apps that ignore the proxy will not be captured
- apps with certificate pinning will not be transparently decrypted
- browser-deep attribution is for managed Chromium sessions, not arbitrary native apps
- the LLM automation layer currently generates plans and artifacts; it is not yet a full autonomous executor that patches arbitrary external systems safely on its own

That said, the repo now contains the full first-pass pipeline for:

- record
- normalize
- map
- analyze
- ask for an automation
- inspect the result through a local UI
