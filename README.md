# agent-mcp-b

`agent-mcp-b` is a terminal-first HTTP(S) interception tool inspired by products like Kampala, but built as an understandable Rust codebase with a clean CLI and app-launch flows.

## Current capabilities

- Run a local interception proxy with focused API-style output
- Capture request method, URL, selected headers, and decoded body previews
- Capture response status, selected headers, and decoded JSON or text bodies
- Filter by host substring, URL substring, and HTTP method
- Emit focused, pretty, or JSON-line output modes
- Generate and persist a local certificate authority for HTTPS interception
- Launch Chrome through the proxy with a managed profile directory
- Attach already-open macOS apps through the system proxy on a chosen network service

## Commands

```bash
cargo run -- paths
cargo run -- ca status
cargo run -- ca trust
cargo run -- proxy --listen 127.0.0.1:8787
cargo run -- chrome --open https://discord.com --insecure-ignore-cert-errors
cargo run -- attach --service Wi-Fi --host-contains discord.com --url-contains /api/
```

Route traffic through the proxy with a tool like `curl`:

```bash
curl --proxy http://127.0.0.1:8787 http://example.com
```

When the proxy starts, it prints the local CA certificate path. Trust that certificate in the client or OS to inspect HTTPS traffic without certificate warnings.

On macOS, trust the local CA in your login keychain with:

```bash
cargo run -- ca trust
```

For a managed Chrome session, the tool launches Chrome with the proxy already configured:

```bash
cargo run -- chrome --listen 127.0.0.1:8787 --open https://example.com
```

Use `--insecure-ignore-cert-errors` only for first-run sessions before the local CA is trusted.

For already-open apps on macOS, use the attach flow:

```bash
cargo run -- attach --listen 127.0.0.1:8787 --service Wi-Fi
```

That temporarily enables the system web and secure web proxies so existing apps can be captured on their next network requests. The previous proxy settings are restored when the command exits.

For Discord-like API capture, the focused mode is usually the cleanest:

```bash
cargo run -- chrome \
  --listen 127.0.0.1:8787 \
  --open https://discord.com \
  --insecure-ignore-cert-errors \
  --host-contains discord.com \
  --url-contains /api/
```
