"""Command line for humans. Same operations the MCP server exposes to agents."""

from __future__ import annotations

import argparse
import json
from pathlib import Path

from . import session as sessions
from . import system
from .call import call
from .export import export


def main() -> None:
    parser = argparse.ArgumentParser("reqtrace", description="Watch an app, learn its API.")
    sub = parser.add_subparsers(dest="command", required=True)

    start = sub.add_parser("start", help="begin a capture")
    start.add_argument("target", help="URL for browser mode, host for proxy mode")
    start.add_argument("--mode", choices=["browser", "proxy"], default="browser")
    start.add_argument("--host", action="append", dest="hosts", help="extra host to intercept")
    start.add_argument("--no-system-proxy", action="store_false", dest="system_proxy")
    start.add_argument("--headless", action="store_true", help="browser mode without a window")
    start.add_argument("--port", type=int, default=0, help="proxy port, or Chrome's debug port")

    sub.add_parser("stop", help="end the capture and infer the API")
    sub.add_parser("sessions", help="list captures")
    sub.add_parser("trust", help="install the local CA into the login keychain")
    sub.add_parser("ca", help="print the CA certificate path, for clients you point at it yourself")

    visit = sub.add_parser("open", help="point the captured browser tab at a URL")
    visit.add_argument("url")

    run = sub.add_parser("eval", help="run JavaScript in the captured browser tab")
    run.add_argument("expression", help="e.g. document.querySelector('button').click()")

    forget = sub.add_parser("rm", help="delete a capture and everything it recorded")
    forget.add_argument("session_id")

    show = sub.add_parser("show", help="show endpoints, or one endpoint in detail")
    show.add_argument("endpoint_id", nargs="?")
    show.add_argument("--session")
    show.add_argument("--full", action="store_true", help="include the raw schemas")

    invoke = sub.add_parser("call", help="call a captured endpoint")
    invoke.add_argument("endpoint_id")
    invoke.add_argument("--param", action="append", default=[], metavar="NAME=VALUE")
    invoke.add_argument("--query", action="append", default=[], metavar="NAME=VALUE")
    invoke.add_argument("--body", help="JSON request body")
    invoke.add_argument("--header", action="append", default=[], metavar="NAME=VALUE")
    invoke.add_argument("--session")

    dump = sub.add_parser("export", help="write openapi.json and a standalone MCP server")
    dump.add_argument("dest")
    dump.add_argument("--session")
    dump.add_argument("--only", action="append", help="export just this endpoint id")

    args = parser.parse_args()
    try:
        print(dispatch(args))
    except RuntimeError as error:
        raise SystemExit(f"reqtrace: {error}")


def dispatch(args: argparse.Namespace) -> str:
    match args.command:
        case "start":
            session = sessions.start(
                args.target,
                mode=args.mode,
                hosts=args.hosts,
                system_proxy=args.system_proxy,
                headless=args.headless,
                port=args.port,
            )
            return f"recording {session.id} ({session.mode}) -> {session.target} on port {session.port}"
        case "stop":
            session = sessions.current()
            if session is None:
                return "nothing is recording"
            api = session.stop()
            return "\n".join(
                [f"{session.id}: {session.seen()} requests -> {len(api.endpoints)} endpoints"]
                + [endpoint.summary() for endpoint in api.endpoints]
            )
        case "sessions":
            return "\n".join(
                f"{s.id}  {'recording' if s.recording else 'stopped':<9} {s.mode:<7} {s.target}"
                for s in sessions.all_sessions()
            ) or "no sessions yet"
        case "trust":
            system.trust_ca(certificate())
            return f"trusted {certificate()}"
        case "ca":
            return str(certificate())
        case "open":
            sessions.resolve(None).navigate(args.url)
            return f"navigated to {args.url}"
        case "eval":
            return sessions.resolve(None).evaluate(args.expression)
        case "rm":
            return f"deleted {sessions.remove(args.session_id)}"
        case "show":
            api = sessions.resolve(args.session).api()
            if not args.endpoint_id:
                return "\n".join(e.summary() for e in api.endpoints) or "no endpoints"
            endpoint = api.get(args.endpoint_id)
            if endpoint is None:
                return "no such endpoint"
            return json.dumps(vars(endpoint), indent=2) if args.full else endpoint.brief()
        case "call":
            result = call(
                sessions.resolve(args.session),
                args.endpoint_id,
                pairs(args.param),
                pairs(args.query),
                json.loads(args.body) if args.body else None,
                pairs(args.header),
            )
            return f"{result['status']} {result['url']}\n{result['body']}"
        case "export":
            session = sessions.resolve(args.session)
            written = export(session, Path(args.dest).expanduser(), args.only)
            return "\n".join(str(path) for path in written)
    return ""


def certificate() -> Path:
    """The CA certificate path, generating the CA if this is the first use."""
    path = sessions.CERTS / "ca-cert.pem"
    if not path.exists():
        system.run(str(sessions.capture_binary()), "ca", "--cert-dir", str(sessions.CERTS))
    return path


def pairs(items: list[str]) -> dict[str, str]:
    return dict(item.split("=", 1) for item in items)


if __name__ == "__main__":
    main()
