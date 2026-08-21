"""Read a HAR file as if we had captured it ourselves.

A HAR is what every browser's DevTools exports from its network panel, so this
is the zero-setup way in: no proxy, no certificate, no launched browser. The
exchanges it yields are the same shape the capture binary emits, which means
inference, replay and export cannot tell the difference.
"""

from __future__ import annotations

import json
from datetime import datetime
from pathlib import Path

from .events import Exchange


def load(path: Path) -> list[Exchange]:
    """Converts every entry in a HAR file into an Exchange."""
    document = json.loads(path.read_text())
    entries = document.get("log", {}).get("entries", [])
    if not entries:
        raise RuntimeError(f"{path} has no entries; is it a HAR file?")
    return [exchange(entry) for entry in entries]


def exchange(entry: dict) -> Exchange:
    request, response = entry.get("request", {}), entry.get("response", {})
    return Exchange(
        t=timestamp(entry.get("startedDateTime")),
        source="har",
        method=request.get("method", "GET"),
        url=request.get("url", ""),
        req_headers=headers(request.get("headers")),
        req_body=(request.get("postData") or {}).get("text"),
        status=response.get("status", 0),
        res_headers=headers(response.get("headers")),
        res_body=(response.get("content") or {}).get("text"),
        ms=int(entry.get("time") or 0),
        # A HAR records what the browser sent, not what the user clicked.
        trigger=None,
    )


def headers(entries: list | None) -> dict[str, str]:
    """HAR stores headers as a list of name/value pairs, last one winning."""
    return {
        item["name"].lower(): item.get("value", "")
        for item in entries or []
        if item.get("name") and not item["name"].startswith(":")
    }


def timestamp(started: str | None) -> float:
    if not started:
        return 0.0
    try:
        return datetime.fromisoformat(started.replace("Z", "+00:00")).timestamp()
    except ValueError:
        return 0.0
