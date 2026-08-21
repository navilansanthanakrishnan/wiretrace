"""Turn an inferred API into something an agent can keep.

Two artifacts: an OpenAPI document, for anything that reads specs, and a
standalone MCP server, for anything that speaks MCP. The generated server is a
single readable file the agent is free to edit.
"""

from __future__ import annotations

import json
import re
from pathlib import Path

from .api import Api, Endpoint, is_secret
from .session import Session, hostname, write_private


def openapi(endpoints: list[Endpoint], host: str) -> dict:
    """An OpenAPI 3.1 document for one host.

    One document per host, because `servers` is a document-level list: a spec
    mixing hosts resolves every path against the first one and is quietly wrong.
    """
    paths: dict[str, dict] = {}
    for endpoint in endpoints:
        operation = {
            "operationId": endpoint.id,
            "summary": endpoint.triggers[0] if endpoint.triggers else f"observed {endpoint.calls}x",
            "parameters": [
                {"name": name, "in": "path", "required": True, "schema": {"type": "string"}}
                for name in endpoint.path_params
            ]
            + [
                {"name": name, "in": "query", "required": False, "schema": {"type": "string"},
                 "example": example}
                for name, example in endpoint.query_params.items()
            ],
            "security": [{header: []} for header in endpoint.auth],
            "responses": {
                str(status): {"content": {"application/json": {"schema": endpoint.response_schema or {}}}}
                for status in endpoint.statuses
            },
        }
        if endpoint.body_schema:
            operation["requestBody"] = {"content": {"application/json": {"schema": endpoint.body_schema}}}
        paths.setdefault(endpoint.path, {})[endpoint.method.lower()] = operation

    return {
        "openapi": "3.1.0",
        "info": {"title": host, "version": "0.1.0"},
        "servers": [{"url": f"https://{host}"}],
        "paths": paths,
    }


def export(session: Session, dest: Path, only: list[str] | None = None) -> list[Path]:
    """Writes the API, its spec, credentials and a runnable MCP server into `dest`.

    Telemetry and liveness probes are excluded. `only` is a full override: name
    an endpoint there and it is exported whatever it is.
    """
    api = session.api()
    endpoints = [e for e in api.endpoints if e.id in only] if only else api.useful()
    if not endpoints:
        raise RuntimeError(f"session {session.id} has no endpoints to export")

    dest.mkdir(parents=True, exist_ok=True)
    hosts = sorted({endpoint.host for endpoint in endpoints})
    written = []

    for host in hosts:
        name = "openapi.json" if len(hosts) == 1 else f"openapi-{host}.json"
        spec = openapi([e for e in endpoints if e.host == host], host)
        written.append(write(dest / name, json.dumps(spec, indent=2)))

    written.append(write(dest / "api.json", json.dumps({"endpoints": [describe(e) for e in endpoints]}, indent=2)))
    # Named for what was captured, not for whichever host sorts first.
    label = hostname(session.target) or hosts[0]
    tools = "\n\n".join(tool_source(endpoint) for endpoint in endpoints)
    written.append(write(dest / "server.py",
                         SERVER_TEMPLATE.format(target=session.target, name=label, tools=tools)))

    # The export is meant to outlive its capture session, so it carries its own
    # copy of the credentials rather than pointing back at one.
    write_private(dest / "credentials.json", credentials_for(session, hosts))
    written.append(dest / "credentials.json")
    return written


def describe(endpoint: Endpoint) -> dict:
    """The endpoint as the generated server needs it, plus a usable description."""
    return {**vars(endpoint), "description": signature(endpoint)}


def credentials_for(session: Session, hosts: list[str]) -> dict:
    path = session.dir / "credentials.json"
    store = json.loads(path.read_text()) if path.exists() else {}
    return {host: store.get(host, {}) for host in hosts}


def write(path: Path, content: str) -> Path:
    path.write_text(content)
    return path


def signature(endpoint: Endpoint) -> str:
    """Human-readable call shape, used as the generated tool's description."""
    parts = [f"{endpoint.method} https://{endpoint.host}{endpoint.path}"]
    for label, names in (
        ("path", endpoint.path_params),
        ("query", list(endpoint.query_params)),
        ("body", list((endpoint.body_schema or {}).get("properties", {}))),
        ("returns", list((endpoint.response_schema or {}).get("properties", {}))),
    ):
        if names:
            parts.append(f"{label}: {', '.join(names)}")
    if endpoint.triggers:
        parts.append(f"seen from: {endpoint.triggers[0]}")
    return " | ".join(parts)


KEYWORDS = {"class", "def", "from", "import", "return", "lambda", "global", "pass",
            "None", "True", "False", "and", "or", "not", "in", "is", "for", "if", "else"}
TYPES = {"string": "str", "number": "float", "boolean": "bool", "object": "dict", "array": "list"}
MAX_FIELDS = 25


def python_name(wire: str, taken: set[str]) -> str:
    """A wire field name as a valid, non-colliding Python parameter."""
    name = re.sub(r"\W", "_", wire).lstrip("0123456789_") or "field"
    if name in KEYWORDS:
        name += "_"
    while name in taken:
        name += "_"
    taken.add(name)
    return name


def tool_source(endpoint: Endpoint) -> str:
    """Generates one MCP tool as real Python, with a real signature.

    The whole point of inferring a body schema is lost if the tool then accepts
    an opaque dict: the calling agent cannot see that `content` is the message.
    So the fields become named parameters, which is also what makes the emitted
    file worth reading.
    """
    taken, required, optional, binds = set(), [], [], {"path": [], "query": [], "body": []}

    for wire in endpoint.path_params:
        name = python_name(wire, taken)
        required.append(f"{name}: str")
        binds["path"].append(f'"{wire}": {name}')

    for wire, schema in list((endpoint.body_schema or {}).get("properties", {}).items())[:MAX_FIELDS]:
        name = python_name(wire, taken)
        optional.append(f"{name}: {TYPES.get(schema.get('type'), 'str')} | None = None")
        binds["body"].append(f'"{wire}": {name}')

    # Credential query parameters are reattached from the private store, so they
    # must not appear in the signature: the caller neither supplies nor sees them.
    public = [wire for wire in endpoint.query_params if not is_secret(wire)]
    for wire in public[:MAX_FIELDS]:
        name = python_name(wire, taken)
        optional.append(f"{name}: str | None = None")
        binds["query"].append(f'"{wire}": {name}')

    signature = ", ".join(required + optional + ["extra: dict | None = None", "limit: int = LIMIT"])
    return TOOL_TEMPLATE.format(
        id=endpoint.id,
        signature=signature,
        doc=docstring(endpoint),
        path="{" + ", ".join(binds["path"]) + "}",
        query="{" + ", ".join(binds["query"]) + "}",
        body="{" + ", ".join(binds["body"]) + "}",
    )


def docstring(endpoint: Endpoint) -> str:
    """Reads as documentation for the call, dependencies included."""
    lines = [f"{endpoint.method} https://{endpoint.host}{endpoint.path}"]
    for edge in endpoint.depends_on:
        lines.append(f"    Needs first: {edge['parameter']} <- "
                     f"{edge['from_endpoint']}.{edge['from_field']}")
    returns = list((endpoint.response_schema or {}).get("properties", {}))[:12]
    if returns:
        lines.append("    Returns: " + ", ".join(returns))
    if endpoint.triggers:
        lines.append(f"    Seen from: {endpoint.triggers[0]}")
    return "\n".join(lines)


TOOL_TEMPLATE = '''@mcp.tool()
def {id}({signature}) -> str:
    """{doc}
    """
    return invoke(
        ENDPOINTS["{id}"],
        {path},
        {query},
        {body},
        extra,
        limit,
    )'''


#: The generated server reads the API description beside it. Each endpoint is a
#: real function with named parameters, so a client sees a usable schema.
SERVER_TEMPLATE = '''#!/usr/bin/env python3
"""MCP server for {target}, generated by wiretrace.

Run it with:  uv run --with mcp --with httpx python server.py
Reads api.json and credentials.json from this directory. Edit freely — nothing
regenerates this file.
"""

import json
import re
import logging
from pathlib import Path

import httpx
from mcp.server.mcpserver import MCPServer

# httpx logs every request URL at INFO, credentials in the query string and all.
logging.getLogger("httpx").setLevel(logging.WARNING)

HERE = Path(__file__).parent
LIMIT = 8000

ENDPOINTS = {{e["id"]: e for e in json.loads((HERE / "api.json").read_text())["endpoints"]}}
_CREDS = HERE / "credentials.json"
CREDENTIALS = json.loads(_CREDS.read_text()) if _CREDS.exists() else {{}}

mcp = MCPServer("{name}")


def invoke(endpoint, path_params, query, body, extra, limit):
    """Anything left as None falls back to what wiretrace captured."""
    store = CREDENTIALS.get(endpoint["host"], {{}})
    given = {{k: v for k, v in body.items() if v is not None}}
    given.update(extra or {{}})

    body = {{**(endpoint.get("body_example") or {{}}), **given}} if given else endpoint.get("body_example")
    query = {{
        **endpoint["query_params"],
        **{{k[1:]: v for k, v in store.items() if k.startswith("?")}},
        **{{k: v for k, v in query.items() if v is not None}},
    }}

    path = endpoint["path"]
    for name, value in path_params.items():
        path = path.replace("{{%s}}" % name, str(value))

    headers = {{
        "accept": "application/json",
        **endpoint.get("headers", {{}}),
        **{{k: v for k, v in store.items() if not k.startswith("?")}},
    }}
    if body is not None:
        headers.setdefault("content-type", "application/json")

    response = httpx.request(
        endpoint["method"],
        f"https://{{endpoint['host']}}{{path}}",
        params=query or None,
        headers=headers,
        content=json.dumps(body) if body is not None else None,
        timeout=30.0,
        follow_redirects=True,
    )
    # A successful call returns the body alone, so it parses as JSON. Truncation
    # would break that, so say so loudly rather than emit a broken document.
    text = response.text
    if len(text) > limit:
        return (f"response was {{len(text)}} characters, over the {{limit}} limit. "
                f"Raise `limit`, or narrow the request.\\n{{text[:limit]}}\\n... truncated")
    return text if response.is_success else f"HTTP {{response.status_code}}\\n{{text}}"


{tools}

if __name__ == "__main__":
    mcp.run()
'''
