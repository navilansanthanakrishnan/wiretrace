# agent-mcp-b

`agent-mcp-b` is a terminal-first HTTP(S) interception system written in Rust. It is designed to capture application-layer traffic in a way that is explicit, inspectable, and production-shaped rather than script-like.

At a high level, the tool runs a local man-in-the-middle proxy, terminates outbound TLS with a locally generated certificate authority, forwards requests upstream, and emits structured request/response captures to the terminal. It supports two routing models on macOS:

- managed browser launch, where Chrome is started with explicit proxy flags
- existing-app attachment, where the macOS system web and secure web proxies are temporarily pointed at the local listener

The intent is to make network workflows legible. Instead of watching a UI and guessing what happened, the tool exposes the underlying HTTP exchange: method, URL, headers, request body, response status, response headers, and decoded response body.

## What It Does

`agent-mcp-b` currently provides:

- local HTTP interception
- local HTTPS interception via a persisted root CA
- per-host leaf certificate generation at proxy time
- request/response capture with decoded body previews
- output filtering by host substring, URL substring, and method
- focused API-style output for high-signal backend traffic
- managed Chrome launch through the proxy
- macOS system-proxy attachment for already-open apps that honor proxy settings

## How It Works

The system is built around a local proxy runtime.

For HTTP traffic:

1. the client sends plaintext HTTP to the local proxy
2. the proxy captures the request
3. the proxy forwards the request upstream
4. the upstream response is captured and returned to the client

For HTTPS traffic:

1. the client opens a `CONNECT host:443` tunnel to the local proxy
2. the proxy generates or reuses a host-specific leaf certificate signed by the local root CA
3. the client establishes TLS with the local proxy instead of directly with the origin
4. the proxy establishes its own outbound TLS connection to the real origin
5. request and response bodies are available in plaintext inside the proxy
6. the proxy forwards the response back to the client

This means the tool is not scraping rendered DOM state. It is observing the underlying application protocol exchange after proxy routing has been established.

## Architecture

The codebase is intentionally split into a few clear layers:

- control plane: CLI parsing, subcommand dispatch, runtime paths, logging
- routing layer: managed Chrome launch or macOS proxy attachment
- transport layer: local HTTP(S) proxy runtime
- trust layer: root CA persistence and host certificate generation
- capture layer: request/response buffering, filtering, decoding, redaction, and output formatting

The important modules are:

- `src/main.rs`: program entrypoint and subcommand dispatch
- `src/cli.rs`: CLI surface and shared command configuration
- `src/app.rs`: persistent runtime directories for certs and logs
- `src/chrome.rs`: managed Chrome session lifecycle
- `src/attach.rs`: macOS system proxy attachment flow
- `src/system_proxy.rs`: `networksetup` integration for reading, setting, and restoring proxy state
- `src/local_ca.rs`: root CA trust management and diagnostics
- `src/proxy/runtime.rs`: proxy bootstrap and lifecycle
- `src/proxy/authority.rs`: CA materialization and per-host TLS certificate generation
- `src/proxy/capture.rs`: interception logic, filtering, body decoding, redaction, and terminal output

## TLS and Trust Model

HTTPS interception depends on a locally trusted root CA.

On first use, the tool creates and persists:

- a root CA certificate
- a corresponding private key

Those files are stored under the app runtime directory:

```bash
cargo run -- paths
```

At request time, the proxy generates a leaf certificate for the target host, for example `discord.com`, signed by the local root CA. The client will only accept that leaf certificate if the local root CA is trusted by the operating system or client runtime.

On macOS, install trust with:

```bash
cargo run -- ca trust
```

Verify trust state with:

```bash
cargo run -- ca status
```

The important line is:

```text
user_trust_contains_ca=true
```

That indicates the CA is present in the user trust domain that Chrome consumes on macOS. After changing trust settings, fully quit and reopen Chrome before testing HSTS sites such as `discord.com`.

## Capture Model

The proxy captures both sides of the exchange:

- request metadata
- request headers
- request body
- response metadata
- response headers
- response body

Bodies are buffered, decoded when compressed, classified, and then rendered as:

- JSON, if valid JSON
- text, if the content looks textual
- binary hex preview otherwise

Sensitive headers are redacted before output. The current redaction list includes:

- `authorization`
- `cookie`
- `set-cookie`
- `x-super-properties`

## Output Modes

The proxy exposes three output modes.

`focused`
- default mode
- prints only higher-signal API-like flows
- suppresses most browser noise such as ordinary page asset fetches

`pretty`
- prints verbose request and response blocks
- useful for manual debugging

`json`
- emits structured JSON-line events
- useful when another tool will consume the capture stream

## Commands

### Print Runtime Paths

```bash
cargo run -- paths
```

### Inspect or Trust the Local CA

```bash
cargo run -- ca status
cargo run -- ca trust
```

### Run the Proxy Directly

```bash
cargo run -- proxy --listen 127.0.0.1:8787
```

You can verify plain routing with a direct client such as `curl`:

```bash
curl --proxy http://127.0.0.1:8787 http://example.com
curl --proxy http://127.0.0.1:8787 https://example.com
```

### Launch a Managed Chrome Session

```bash
cargo run -- chrome --listen 127.0.0.1:8787 --open https://example.com
```

This command:

- starts the proxy in-process
- waits for the listener to become ready
- launches Chrome with explicit proxy flags
- uses an isolated profile by default
- tears the session down when Chrome exits or the command is interrupted

For first-run debugging before the CA is trusted, you can temporarily disable browser certificate enforcement:

```bash
cargo run -- chrome \
  --listen 127.0.0.1:8787 \
  --open https://example.com \
  --insecure-ignore-cert-errors
```

That flag is a bootstrap/debug path, not the normal operating mode.

### Attach Already-Open macOS Apps

```bash
cargo run -- attach --listen 127.0.0.1:8787 --service Wi-Fi
```

This command:

- snapshots the current macOS web and secure web proxy settings
- points both settings at the local proxy
- starts the local proxy
- restores the original settings when the process exits

Apps must make new requests after attachment to be captured. In practice that usually means reloading the page, navigating again, or restarting the app.

## Filtering

Capture output can be constrained with:

- `--host-contains`
- `--url-contains`
- `--methods`

Example:

```bash
cargo run -- chrome \
  --listen 127.0.0.1:8787 \
  --open https://discord.com \
  --host-contains discord.com \
  --url-contains /api/
```

This is typically the cleanest way to observe Discord-style application traffic without seeing every unrelated browser request.

## Operational Notes

If you see:

```text
listen address 127.0.0.1:8787 is already in use
```

another process is already bound to that port. Stop the existing process or choose a different `--listen` value.

If HTTPS sites show a privacy error:

- confirm `cargo run -- ca status` reports `user_trust_contains_ca=true`
- fully quit and reopen Chrome
- ensure the app is actually routed through the proxy

If `attach` mode appears to do nothing:

- the app may not honor macOS system proxy settings
- the app may need to be reloaded or restarted
- the target traffic may be certificate pinned or may not be HTTP(S)

## Current Limitations

This project is intentionally narrow right now. It is not yet:

- a transparent system-wide packet interceptor
- a certificate pinning bypass system
- a process-aware traffic attribution engine
- a request replay engine
- a workflow extraction engine
- a long-term capture persistence or indexing system

It works best today for:

- browsers
- Electron apps
- desktop software that honors system proxy settings
- standard HTTP/HTTPS workflows without certificate pinning

## Development

Build and test with:

```bash
cargo check
cargo test
```

The project currently uses:

- `tokio` for async runtime and process orchestration
- `hudsucker` for proxy interception
- `clap` for CLI parsing
- `serde` and `serde_json` for structured output
- `rcgen` and OpenSSL-backed certificate construction for the local CA and leaf cert pipeline
- `brotli` and `flate2` for compressed body decoding
