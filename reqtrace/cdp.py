"""Driving the managed Chrome, so an unattended agent can work a page.

The capture binary owns its own CDP connection for *reading* traffic. This is a
second, deliberately tiny client for *writing* — navigate and evaluate, nothing
else. Chrome accepts multiple DevTools clients, so the two never contend.
"""

from __future__ import annotations

import json
import urllib.request

from websockets.sync.client import connect

TIMEOUT = 15


def target(port: int) -> str:
    """The websocket URL of the page the capture is attached to (the first one)."""
    listing = urllib.request.urlopen(f"http://127.0.0.1:{port}/json", timeout=TIMEOUT)
    pages = [t for t in json.load(listing) if t.get("type") == "page"]
    if not pages:
        raise RuntimeError(f"no chrome page on port {port}; is a browser capture running?")
    return pages[0]["webSocketDebuggerUrl"]


def command(port: int, method: str, params: dict) -> dict:
    with connect(target(port), open_timeout=TIMEOUT) as socket:
        socket.send(json.dumps({"id": 1, "method": method, "params": params}))
        reply = json.loads(socket.recv(timeout=TIMEOUT))
    if "error" in reply:
        raise RuntimeError(f"{method} failed: {reply['error']}")
    return reply.get("result", {})


def navigate(port: int, url: str) -> None:
    command(port, "Page.navigate", {"url": url})


def evaluate(port: int, expression: str) -> str:
    """Runs JavaScript in the page — click a button, fill a field, scroll."""
    result = command(port, "Runtime.evaluate", {"expression": expression, "awaitPromise": True})
    if "exceptionDetails" in result:
        return f"error: {result['exceptionDetails'].get('text', 'evaluation failed')}"
    return json.dumps(result.get("result", {}).get("value"))
