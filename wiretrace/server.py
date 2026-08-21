"""wiretrace watches an application and tells you what API it is really calling.

The loop: start_capture, use the app, stop_capture, then read the endpoints it
inferred, call one for real, and keep the result as a standalone MCP server.

wiretrace only observes. It does not click, type or navigate — drive the app with
the computer-use skill (open-computer-use), which is linked at
`open-computer-use/` in this repo. Run captures with a visible window so it has
something to drive.

Captured credentials stay in a private store and are reattached on replay, so a
captured endpoint can be called as the logged-in user without the token ever
being returned to you.
"""

from __future__ import annotations

import json
from pathlib import Path

from mcp.server.mcpserver import MCPServer

from . import session as sessions
from . import system
from .call import call, probe
from .export import export, signature

mcp = MCPServer("wiretrace", instructions=__doc__)


@mcp.tool()
def start_capture(
    target: str,
    mode: str = "browser",
    hosts: list[str] | None = None,
    system_proxy: bool = True,
    headless: bool = False,
    port: int = 0,
) -> str:
    """Start observing an application's HTTP traffic.

    mode="browser": launches a managed Chrome at `target` and attributes each
    request to the click that caused it. Drive it with open_url or by clicking.
    mode="proxy": intercepts native apps and scripts. `target` is the host to
    watch and is always intercepted; `hosts` adds more. Set system_proxy=False
    to leave macOS network settings alone and route traffic yourself — that is
    the unattended path, and it is how you capture a script or a CLI you drive.

    Use the app normally, then call stop_capture. Driving the UI is not this
    tool's job: use the computer-use skill against the window. headless=True
    leaves no window to drive, so it only suits a capture that needs nothing
    beyond loading `target`.
    """
    session = sessions.start(
        target,
        mode=mode,
        hosts=hosts,
        system_proxy=system_proxy,
        headless=headless,
        port=port,
    )
    hint = (
        "chrome is open; drive it with the computer-use skill, then stop_capture"
        if mode == "browser"
        else route_hint(session, system_proxy)
    )
    return f"recording session {session.id} ({mode}) -> {session.target}\n{hint}"


@mcp.tool()
def import_har(path: str) -> str:
    """Infer an API from a HAR file exported from browser DevTools.

    No capture, no proxy, no certificate — use this when the user can hand you a
    .har, or when they have already reproduced the flow in their own browser.
    The result is an ordinary session: describe, call and export it as usual.
    """
    session = sessions.ingest(Path(path).expanduser())
    api = session.api()
    listing = "\n".join(endpoint.summary() for endpoint in api.endpoints[:40])
    return f"session {session.id}: {len(api.endpoints)} endpoints from {path}\n{listing}"


@mcp.tool()
def stop_capture() -> str:
    """Stop the running capture and infer the API from what was observed."""
    session = sessions.current()
    if session is None:
        return "nothing is recording; start_capture first"
    api = session.stop()
    if not api.endpoints:
        return f"session {session.id}: {session.seen()} requests captured, none of them API calls.\n{diagnose(session)}"
    listing = "\n".join(endpoint.summary() for endpoint in api.endpoints[:40])
    return f"session {session.id}: {len(api.endpoints)} endpoints across {', '.join(api.hosts)}\n{listing}"


@mcp.tool()
def list_endpoints(session_id: str | None = None) -> str:
    """List the endpoints inferred from a capture (defaults to the latest)."""
    session = sessions.resolve(session_id)
    api = session.api()
    return "\n".join(endpoint.summary() for endpoint in api.endpoints) or "no endpoints"


@mcp.tool()
def describe_endpoint(endpoint_id: str, session_id: str | None = None, full: bool = False) -> str:
    """Show one endpoint's parameters and fields. full=True adds the raw schemas."""
    endpoint = sessions.resolve(session_id).api().get(endpoint_id)
    if endpoint is None:
        return f"no endpoint {endpoint_id!r}; try list_endpoints"
    return json.dumps(vars(endpoint), indent=2) if full else endpoint.brief()


@mcp.tool()
def call_endpoint(
    endpoint_id: str,
    path_params: dict | None = None,
    query: dict | None = None,
    body: dict | None = None,
    headers: dict | None = None,
    session_id: str | None = None,
) -> str:
    """Call a captured endpoint for real, reusing the session's captured auth.

    Every argument falls back, field by field, to what was captured: with no
    arguments this reproduces the observed request, and `body={"q": "x"}`
    changes one field of it. `headers` overrides a captured request header.
    """
    session = sessions.resolve(session_id)
    result = call(session, endpoint_id, path_params, query, body, headers)
    return f"{result['status']} {result['url']}\n{result['body']}"


@mcp.tool()
def verify_session(session_id: str | None = None) -> str:
    """Check the captured credentials still authenticate, before relying on them.

    Replays the safest observed read. Do this before exporting an MCP server for
    someone else to use, and whenever a call starts returning 401 or 403.
    """
    return probe(sessions.resolve(session_id))


@mcp.tool()
def export_mcp(dest: str, session_id: str | None = None, only: list[str] | None = None) -> str:
    """Write api.json, openapi.json and a runnable MCP server for this API.

    Third-party telemetry is left out. Pass `only` to export a chosen subset.
    """
    session = sessions.resolve(session_id)
    written = export(session, Path(dest).expanduser(), only)
    endpoints = [e for e in session.api().endpoints if e.id in only] if only else session.api().useful()
    return "\n".join(
        [f"wrote {len(endpoints)} endpoints to:", *(f"  {path}" for path in written), "", "tools:"]
        + [f"  {endpoint.id} — {signature(endpoint)}" for endpoint in endpoints[:20]]
    )


@mcp.tool()
def ca_path() -> str:
    """Path to the capture CA certificate, for clients you point at it yourself.

    Use it to route a script or CLI through a proxy capture without touching the
    keychain: `curl -x http://127.0.0.1:<port> --cacert <this> https://...`
    """
    certificate = sessions.CERTS / "ca-cert.pem"
    if not certificate.exists():
        system.run(str(sessions.capture_binary()), "ca", "--cert-dir", str(sessions.CERTS))
    return str(certificate)


@mcp.tool()
def forget_session(session_id: str) -> str:
    """Delete a capture and everything it recorded, including its credentials."""
    return f"deleted {sessions.remove(session_id)}"


@mcp.tool()
def list_sessions() -> str:
    """List past captures, newest first."""
    return "\n".join(
        f"{s.id}  {'recording' if s.recording else 'stopped':<9} {s.mode:<7} {s.target}"
        for s in sessions.all_sessions()
    ) or "no sessions yet"


def route_hint(session, system_proxy: bool) -> str:
    if system_proxy:
        return f"macOS system proxy now points at 127.0.0.1:{session.port}; use the app, then stop_capture"
    return (
        f"proxy on 127.0.0.1:{session.port}. Route traffic through it yourself, trusting the "
        f"capture CA:  curl -x http://127.0.0.1:{session.port} --cacert {sessions.CERTS}/ca-cert.pem ..."
    )


def diagnose(session) -> str:
    if session.seen() == 0:
        return "Nothing reached the capture at all — the app was not routed through it, or was never used."
    return "Traffic was captured but it was all pages and assets. The app may render server-side."


def main() -> None:
    mcp.run()


if __name__ == "__main__":
    main()
