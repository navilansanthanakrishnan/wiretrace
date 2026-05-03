# agent-mcp-b

`agent-mcp-b` is a terminal-first HTTP(S) interception tool inspired by products like Kampala, but built as an understandable Rust codebase with a clean CLI and app-launch flows.

## Planned capabilities

- Run a local interception proxy with structured request and response output
- Launch Chrome through the proxy for a managed capture session
- Support request filtering, body preview limits, and machine-readable JSON output
- Keep certificate material and runtime state in predictable local paths

## Commands

```bash
cargo run -- paths
cargo run -- proxy --listen 127.0.0.1:8787
cargo run -- chrome --open https://discord.com
```

The first commit establishes the production-shaped CLI and runtime directories. Subsequent commits add the proxy engine, Chrome launcher, and request capture pipeline.
