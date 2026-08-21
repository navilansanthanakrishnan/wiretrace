"""Recover which calls feed which.

A captured request usually carries values it did not invent: a channel id that
came from a guild listing, a CSRF token from a page load, an upload URL from a
create call. Replaying such a call in isolation works right up until that value
goes stale, and then it fails in a way that looks like broken auth.

So we recover the edge. Every response is indexed by the scalar values it
produced; every later request is checked against that index. A hit means this
call consumes that call's output, and the agent is told to fetch it first.

This is a dataflow analysis over the capture, not a guess: the value either
appeared in an earlier response or it did not.
"""

from __future__ import annotations

from .events import Exchange

#: Shorter values collide by accident — "1", "true", "en" prove nothing.
MIN_LENGTH = 8
#: Cap the index so one enormous response cannot dominate memory.
MAX_INDEXED = 5000
MAX_DEPTH = 6


def walk(value: object, path: str = "", depth: int = 0):
    """Yields (json path, scalar as text) for every leaf, arrays collapsed to `[]`."""
    if depth > MAX_DEPTH:
        return
    if isinstance(value, dict):
        for key, item in value.items():
            yield from walk(item, f"{path}.{key}" if path else key, depth + 1)
    elif isinstance(value, list):
        for item in value[:20]:
            yield from walk(item, f"{path}[]", depth + 1)
    elif isinstance(value, (str, int)) and not isinstance(value, bool):
        text = str(value)
        if len(text) >= MIN_LENGTH:
            yield path, text


def consumed(exchange: Exchange, endpoint) -> list[tuple[str, str]]:
    """The values this request supplied: path parameters, query, body leaves."""
    values: list[tuple[str, str]] = []

    template = endpoint.path.split("/")
    actual = exchange.path.split("/")
    for name, position in ((s[1:-1], i) for i, s in enumerate(template) if s.startswith("{")):
        if position < len(actual):
            values.append((name, actual[position]))

    for pair in exchange.query.split("&"):
        if "=" in pair:
            key, _, value = pair.partition("=")
            values.append((f"query.{key}", value))

    body = exchange.json_body("req")
    if body is not None:
        values.extend((f"body.{path}", value) for path, value in walk(body))

    return [(name, value) for name, value in values if len(value) >= MIN_LENGTH]


def link(observations: list[tuple[Exchange, object]]) -> None:
    """Annotates each endpoint with the calls whose output it consumes."""
    origins: dict[str, tuple[object, str]] = {}

    for exchange, endpoint in observations:
        for name, value in consumed(exchange, endpoint):
            source, field = origins.get(value, (None, ""))
            if source is None or source is endpoint:
                continue
            edge = {"parameter": name, "from_endpoint": source.id, "from_field": field}
            if edge not in endpoint.depends_on:
                endpoint.depends_on.append(edge)

        if len(origins) < MAX_INDEXED:
            response = exchange.json_body("res")
            if response is not None:
                for path, value in walk(response):
                    origins.setdefault(value, (endpoint, path))
