"""Replay an inferred endpoint against the live service.

Captured credentials are read from the session's private store and attached
here. The agent supplies parameters; it never sees the token.
"""

from __future__ import annotations

import json
import logging

import httpx

from .api import Endpoint, sanitize
from .session import Session

# httpx logs every request URL at INFO, credentials in the query string and all.
logging.getLogger("httpx").setLevel(logging.WARNING)

RESULT_LIMIT = 8000
TRUNCATED = "\n... truncated"


def credentials(session: Session, host: str) -> tuple[dict[str, str], dict[str, str]]:
    """Captured (headers, query parameters) for a host. Query strings carry auth
    as often as headers do, so both are stored and both are reattached."""
    path = session.dir / "credentials.json"
    store = json.loads(path.read_text()).get(host, {}) if path.exists() else {}
    headers = {k: v for k, v in store.items() if not k.startswith("?")}
    query = {k[1:]: v for k, v in store.items() if k.startswith("?")}
    return headers, query


def call(
    session: Session,
    endpoint_id: str,
    path_params: dict[str, str] | None = None,
    query: dict[str, str] | None = None,
    body: dict | None = None,
    headers: dict[str, str] | None = None,
    timeout: float = 30.0,
) -> dict:
    endpoint = session.api().get(endpoint_id)
    if endpoint is None:
        raise RuntimeError(f"no endpoint {endpoint_id!r} in session {session.id}")

    auth_headers, auth_query = credentials(session, endpoint.host)

    # Anything the caller leaves out falls back to what was captured, field by
    # field, so `call(endpoint)` reproduces the observed request and
    # `call(endpoint, body={"query": "x"})` changes one field of it.
    url = fill(endpoint, path_params or {})
    query = {**endpoint.query_params, **auth_query, **(query or {})}
    body = {**(endpoint.body_example or {}), **body} if body else endpoint.body_example

    sent = {"accept": "application/json", **endpoint.headers, **auth_headers, **(headers or {})}
    if body is not None:
        sent.setdefault("content-type", "application/json")

    response = httpx.request(
        endpoint.method,
        url,
        params=query or None,
        headers=sent,
        content=json.dumps(body) if body is not None else None,
        timeout=timeout,
        follow_redirects=True,
    )
    return {
        "status": response.status_code,
        # Sanitized: the resolved URL carries any credential that travels as a
        # query parameter, and reporting it back would hand the agent the token
        # this whole design exists to keep out of its hands.
        "url": sanitize(str(response.url)),
        "body": summarize(response.text),
    }


def fill(endpoint: Endpoint, values: dict[str, str]) -> str:
    """Substitutes `{name}` path parameters, falling back to values seen in capture."""
    path = endpoint.path
    for name in endpoint.path_params:
        value = values.get(name) or observed(endpoint, name)
        if value is None:
            raise RuntimeError(f"missing path parameter {name!r} for {endpoint.id}")
        path = path.replace("{%s}" % name, str(value))
    return f"https://{endpoint.host}{path}"


def observed(endpoint: Endpoint, name: str) -> str | None:
    """The value that stood in for this parameter during capture."""
    if not endpoint.examples:
        return None
    index = endpoint.path.split("/").index("{%s}" % name)
    captured = endpoint.examples[0].split("://", 1)[-1]
    segments = ("/" + captured.split("?", 1)[0].split("/", 1)[-1]).split("/")
    return segments[index] if index < len(segments) else None


def summarize(text: str) -> str:
    """Pretty-prints JSON when possible and caps the size an agent has to read."""
    try:
        text = json.dumps(json.loads(text), indent=2)
    except ValueError:
        pass
    return text if len(text) <= RESULT_LIMIT else text[:RESULT_LIMIT] + TRUNCATED
