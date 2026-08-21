"""Turn a pile of captured exchanges into an API description.

This is the inference step: group requests that are the same call with different
arguments, work out which path segments are parameters, and infer schemas from
the bodies actually observed. Nothing here talks to a model — it is all derived
from the traffic, which is why the result is reproducible.
"""

from __future__ import annotations

import re
from dataclasses import asdict, dataclass, field
from urllib.parse import unquote

from .events import Exchange

#: Any header or query parameter whose name contains one of these is treated as
#: a credential: the value goes to the private store, never into the API
#: description. Query strings matter as much as headers — plenty of services
#: authenticate with `?api_key=`.
SECRET_WORDS = ("token", "key", "auth", "secret", "session", "password", "sig", "credential")
SECRET_HEADERS = {"cookie", "authorization"}

#: Non-secret request headers worth resending, because private APIs often reject
#: a request that arrives without the client's own user agent — or, in the case
#: of `accept`, answer a content negotiation the caller never asked for.
REPLAY_HEADERS = ("accept", "user-agent", "accept-language", "x-requested-with", "origin", "referer")

#: Third-party telemetry. Captured for completeness, excluded from exports —
#: nobody wants a generated tool that fires analytics beacons.
NOISE_HOSTS = ("google-analytics.com", "googletagmanager.com", "doubleclick.net",
               "segment.io", "sentry.io", "amplitude.com", "mixpanel.com", "telemetry.")

#: Liveness probes. Real endpoints, no use as a tool.
NOISE_PATHS = ("/isalive", "/health", "/healthz", "/ping", "/_health", "/livez", "/readyz")

MAX_DEPTH = 4
MAX_EXAMPLES = 2
REDACTED = "<captured>"

UUID = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$", re.I)
HEX = re.compile(r"^[0-9a-f]{16,}$", re.I)


def is_secret(name: str) -> bool:
    name = name.lower()
    return name in SECRET_HEADERS or any(word in name for word in SECRET_WORDS)


def looks_like_id(segment: str) -> bool:
    """A path segment that identifies a resource rather than naming one.

    Deliberately conservative. Merging two endpoints that are not the same call
    is far worse than leaving two endpoints unmerged, so a segment has to look
    machine-generated — `/repos/{owner}/{repo}`-style human names stay literal.
    """
    return bool(segment) and (
        segment.isdigit() and len(segment) >= 4
        or bool(UUID.match(segment))
        or bool(HEX.match(segment))
        or (len(segment) >= 12 and any(c.isdigit() for c in segment) and "." not in segment)
    )


def singular(word: str) -> str:
    """`channels` -> `channel`, `repositories` -> `repository`; `address`, `status` unchanged."""
    if word.endswith("ies"):
        return word[:-3] + "y"
    if word.endswith("sses") or word.endswith("ches"):
        return word[:-2]
    plural = word.endswith("s") and not word.endswith(("ss", "us", "is"))
    return word[:-1] if plural else word


def templatize(path: str) -> tuple[str, list[str]]:
    """`/v9/channels/8412/messages` -> (`/v9/channels/{channel_id}/messages`, [channel_id])."""
    segments, names = path.split("/"), []
    for index, segment in enumerate(segments):
        if not looks_like_id(segment):
            continue
        previous = singular(segments[index - 1]) if index else ""
        name = re.sub(r"[^a-z0-9_]", "_", f"{previous or 'resource'}_id".lower())
        while name in names:
            name += "_2"
        names.append(name)
        segments[index] = "{%s}" % name
    return "/".join(segments), names


def schema_of(value: object, depth: int = 0) -> dict:
    """A minimal JSON Schema for one observed value."""
    if depth >= MAX_DEPTH:
        return {}
    if isinstance(value, dict):
        return {"type": "object", "properties": {k: schema_of(v, depth + 1) for k, v in value.items()}}
    if isinstance(value, list):
        return {"type": "array", "items": schema_of(value[0], depth + 1) if value else {}}
    if isinstance(value, bool):
        return {"type": "boolean"}
    if isinstance(value, (int, float)):
        return {"type": "number"}
    if value is None:
        return {"type": "null"}
    return {"type": "string"}


def merge(left: dict | None, right: dict | None) -> dict | None:
    """Union of two inferred schemas; later samples can only add fields."""
    if not left or not right:
        return left or right
    if left.get("type") != right.get("type"):
        return left
    merged = dict(left)
    if left.get("type") == "object":
        properties = dict(left.get("properties", {}))
        for key, value in right.get("properties", {}).items():
            properties[key] = merge(properties.get(key), value)
        merged["properties"] = properties
    elif left.get("type") == "array":
        merged["items"] = merge(left.get("items"), right.get("items")) or {}
    return merged


@dataclass
class Endpoint:
    id: str
    method: str
    host: str
    path: str
    path_params: list[str] = field(default_factory=list)
    #: Observed query parameters. Secret values are replaced with `<captured>`.
    query_params: dict[str, str] = field(default_factory=dict)
    #: Non-secret request headers to resend on replay.
    headers: dict[str, str] = field(default_factory=dict)
    body_schema: dict | None = None
    #: One observed request body, so a captured call can be replayed as-is.
    body_example: dict | None = None
    response_schema: dict | None = None
    #: Names only — the values live in the session's private credential store.
    auth: list[str] = field(default_factory=list)
    statuses: list[int] = field(default_factory=list)
    calls: int = 0
    triggers: list[str] = field(default_factory=list)
    examples: list[str] = field(default_factory=list)

    @property
    def url_template(self) -> str:
        return f"https://{self.host}{self.path}"

    @property
    def noise(self) -> bool:
        """Third-party telemetry and liveness probes: captured, never exported."""
        return any(marker in self.host for marker in NOISE_HOSTS) or self.path.endswith(NOISE_PATHS)

    def summary(self) -> str:
        trigger = f"  <- {self.triggers[0]}" if self.triggers else ""
        return f"{self.id}: {self.method} {self.url_template} ({self.calls}x){trigger}"

    def brief(self) -> str:
        """What an agent needs to call it, without the full response schema."""
        lines = [f"{self.id}: {self.method} {self.url_template}",
                 f"  observed {self.calls}x, statuses {self.statuses}"]
        for label, names in (
            ("path params", self.path_params),
            ("query", list(self.query_params)),
            ("body fields", list((self.body_schema or {}).get("properties", {}))),
            ("returns", list((self.response_schema or {}).get("properties", {}))),
            ("auth", self.auth),
            ("triggered by", self.triggers),
        ):
            if names:
                lines.append(f"  {label}: {', '.join(map(str, names))}")
        return "\n".join(lines)


@dataclass
class Api:
    endpoints: list[Endpoint] = field(default_factory=list)
    #: host -> name -> value. Header names are bare; query parameters are
    #: prefixed with `?`. Written to a private file, never shown to agents.
    credentials: dict[str, dict[str, str]] = field(default_factory=dict)

    @property
    def hosts(self) -> list[str]:
        return sorted({endpoint.host for endpoint in self.endpoints})

    def get(self, endpoint_id: str) -> Endpoint | None:
        return next((e for e in self.endpoints if e.id == endpoint_id), None)

    def useful(self) -> list["Endpoint"]:
        """Everything except third-party telemetry."""
        return [endpoint for endpoint in self.endpoints if not endpoint.noise]

    def to_dict(self) -> dict:
        return {"endpoints": [asdict(e) for e in self.endpoints]}

    @classmethod
    def from_dict(cls, raw: dict) -> "Api":
        return cls(endpoints=[Endpoint(**e) for e in raw.get("endpoints", [])])


def infer(exchanges: list[Exchange]) -> Api:
    """Fold captured exchanges into one endpoint per distinct call shape."""
    api, by_key = Api(), {}

    for exchange in exchanges:
        if not exchange.is_api():
            continue
        path, names = templatize(exchange.path)
        key = (exchange.method, exchange.host, path)

        endpoint = by_key.get(key)
        if endpoint is None:
            endpoint = by_key[key] = Endpoint(
                id=identifier(exchange.method, path),
                method=exchange.method,
                host=exchange.host,
                path=path,
                path_params=names,
            )
            api.endpoints.append(endpoint)

        endpoint.calls += 1
        if exchange.status not in endpoint.statuses:
            endpoint.statuses.append(exchange.status)
        if len(endpoint.examples) < MAX_EXAMPLES:
            endpoint.examples.append(sanitize(exchange.url))
        if endpoint.body_example is None:
            body = exchange.json_body("req")
            endpoint.body_example = body if isinstance(body, dict) else None
        endpoint.body_schema = merge(endpoint.body_schema, schema_for(exchange, "req"))
        endpoint.response_schema = merge(endpoint.response_schema, schema_for(exchange, "res"))

        store = api.credentials.setdefault(exchange.host, {})
        for name, value in pairs(exchange.query):
            if is_secret(name):
                store[f"?{name}"] = value
                endpoint.query_params.setdefault(name, REDACTED)
            else:
                endpoint.query_params.setdefault(name, value)

        for header, value in exchange.req_headers.items():
            if is_secret(header):
                store[header] = value
                if header not in endpoint.auth:
                    endpoint.auth.append(header)
            elif header in REPLAY_HEADERS or header.startswith("x-"):
                endpoint.headers.setdefault(header, value)

        if exchange.trigger:
            label = f"{exchange.trigger['kind']} on {exchange.trigger['label']}"
            if label not in endpoint.triggers:
                endpoint.triggers.append(label)

    for endpoint in api.endpoints:
        endpoint.body_example = redact(endpoint.body_example)
    disambiguate(api.endpoints)
    api.endpoints.sort(key=lambda e: (e.noise, -e.calls, e.id))
    return api


def disambiguate(endpoints: list[Endpoint]) -> None:
    """Qualifies every id that several hosts would otherwise share.

    Done after the fact so ids do not depend on which host answered first — four
    CDN nodes serving `/1/isalive` must not race for the unqualified name.
    """
    clashing = {
        endpoint.id
        for endpoint in endpoints
        if sum(other.id == endpoint.id for other in endpoints) > 1
    }
    for endpoint in endpoints:
        if endpoint.id in clashing:
            endpoint.id = f"{slug(endpoint.host.split('.')[0])}_{endpoint.id}"


def schema_for(exchange: Exchange, which: str) -> dict | None:
    body = exchange.json_body(which)
    return schema_of(body) if body is not None else None


def pairs(query: str) -> list[tuple[str, str]]:
    """Decoded query pairs, so replay re-encodes them exactly once."""
    return [
        (name, unquote(value))
        for name, value in (part.split("=", 1) for part in query.split("&") if "=" in part)
    ]


def sanitize(url: str) -> str:
    """An example URL with secret query values blanked.

    Example URLs are agent-visible, and a query string is a perfectly ordinary
    place to find an API key.
    """
    base, _, query = url.partition("?")
    if not query:
        return url
    kept = [
        f"{name}={REDACTED if is_secret(name) else value}"
        for name, value in (part.split("=", 1) if "=" in part else (part, "") for part in query.split("&"))
    ]
    return f"{base}?{'&'.join(kept)}"


def redact(body: dict | None) -> dict | None:
    """Blanks secrets in an example body so it is safe to show."""
    if body is None:
        return None
    return {key: REDACTED if is_secret(key) else value for key, value in body.items()}


def identifier(method: str, path: str) -> str:
    """A short, agent-readable name like `post_channels_messages`."""
    words = [w for w in re.sub(r"\{[^}]*\}", "", path).split("/") if w and not w.startswith("v")]
    return slug("_".join([method, *words[-3:]]))


def slug(text: str) -> str:
    return re.sub(r"[^a-z0-9]+", "_", text.lower()).strip("_")
