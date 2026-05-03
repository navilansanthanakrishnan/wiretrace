# agent-mcp-b

`agent-mcp-b` is a terminal-first HTTP(S) interception tool inspired by products like Kampala, but built as an understandable Rust codebase with a clean CLI and app-launch flows.

## Current capabilities

- Run a local interception proxy with structured request and response output
- Capture request method, URL, headers, and body previews
- Capture response status, headers, and body previews
- Filter by host substring, URL substring, and HTTP method
- Emit either human-readable terminal output or JSON lines
- Generate and persist a local certificate authority for HTTPS interception
- Launch Chrome through the proxy with a managed profile directory

## Commands

```bash
cargo run -- paths
cargo run -- proxy --listen 127.0.0.1:8787
cargo run -- chrome --open https://discord.com --insecure-ignore-cert-errors
```

Route traffic through the proxy with a tool like `curl`:

```bash
curl --proxy http://127.0.0.1:8787 http://example.com
```

When the proxy starts, it prints the local CA certificate path. Trust that certificate in the client or OS to inspect HTTPS traffic without certificate warnings.

For a managed Chrome session, the tool launches Chrome with the proxy already configured:

```bash
cargo run -- chrome --listen 127.0.0.1:8787 --open https://example.com
```

Use `--insecure-ignore-cert-errors` only for first-run sessions before the local CA is trusted.
