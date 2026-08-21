"""An optional localhost UI for looking at what was captured.

The agent is the primary consumer; this exists for the times you want to read a
capture yourself. Deliberately one file of stdlib HTTP and one file of HTML —
no framework, no build step, nothing to install — because a viewer that needs
its own toolchain is a worse deal than reading the JSON.
"""

from __future__ import annotations

import json
from dataclasses import asdict
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from . import session as sessions
from .call import call

PAGE = Path(__file__).parent / "ui.html"


def serve(host: str = "127.0.0.1", port: int = 4317) -> None:
    server = ThreadingHTTPServer((host, port), Handler)
    print(f"wiretrace ui on http://{host}:{port}  (ctrl-c to stop)")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        server.shutdown()


class Handler(BaseHTTPRequestHandler):
    def do_GET(self) -> None:
        if self.path == "/":
            return self.send(PAGE.read_bytes(), "text/html; charset=utf-8")
        if self.path == "/api/sessions":
            return self.json([summary(s) for s in sessions.all_sessions()])
        if self.path.startswith("/api/sessions/"):
            return self.json(detail(self.path.rsplit("/", 1)[-1]))
        self.send(b"not found", "text/plain", status=404)

    def do_POST(self) -> None:
        if self.path != "/api/call":
            return self.send(b"not found", "text/plain", status=404)
        length = int(self.headers.get("content-length", 0))
        request = json.loads(self.rfile.read(length) or b"{}")
        try:
            result = call(
                sessions.resolve(request.get("session")),
                request["endpoint"],
                request.get("path_params"),
                request.get("query"),
                request.get("body"),
            )
        except Exception as error:
            result = {"status": 0, "url": "", "body": str(error)}
        self.json(result)

    def json(self, payload: object) -> None:
        self.send(json.dumps(payload).encode(), "application/json")

    def send(self, body: bytes, content_type: str, status: int = 200) -> None:
        self.send_response(status)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_) -> None:
        """Quiet: the interesting output is the capture, not the viewer."""


def summary(session: sessions.Session) -> dict:
    return {
        "id": session.id,
        "target": session.target,
        "mode": session.mode,
        "recording": session.recording,
    }


def detail(session_id: str) -> dict:
    session = sessions.Session.load(session_id)
    api = session.api()
    return {
        **summary(session),
        "requests": session.seen(),
        # asdict is safe here: credentials live in a separate file and endpoint
        # fields only ever hold names, never captured secret values.
        # `noise` is a property, so it needs adding by hand — the viewer dims
        # telemetry and health checks rather than hiding them, since "what else
        # did this app call" is half of why you would open this page.
        "endpoints": [{**asdict(e), "noise": e.noise} for e in api.endpoints],
    }
