"""The captured exchange, as produced by the Rust capture binary."""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import urlsplit

STATIC_SUFFIXES = (".js", ".css", ".png", ".jpg", ".svg", ".woff", ".woff2", ".ico", ".map")


@dataclass(frozen=True)
class Exchange:
    t: float
    source: str
    method: str
    url: str
    req_headers: dict[str, str]
    req_body: str | None
    status: int
    res_headers: dict[str, str]
    res_body: str | None
    ms: int
    trigger: dict[str, str] | None

    @classmethod
    def from_json(cls, raw: dict) -> "Exchange":
        """Strict: a line missing fields is a broken line, not a blank Exchange."""
        return cls(**{field: raw[field] for field in cls.__annotations__})

    @property
    def host(self) -> str:
        return urlsplit(self.url).netloc

    @property
    def path(self) -> str:
        return urlsplit(self.url).path or "/"

    @property
    def query(self) -> str:
        return urlsplit(self.url).query

    def json_body(self, which: str) -> object | None:
        """Parsed request or response body, or None when it is not JSON."""
        body = self.req_body if which == "req" else self.res_body
        try:
            return json.loads(body) if body else None
        except ValueError:
            return None

    def is_api(self) -> bool:
        """Whether this looks like an application API call rather than page furniture.

        Everything downstream works on API calls only; this one predicate is what
        keeps an inferred API from drowning in asset requests.
        """
        if self.status == 0 or self.path.endswith(STATIC_SUFFIXES):
            return False
        content_type = self.res_headers.get("content-type", "")
        return (
            self.method != "GET"
            or "json" in content_type
            or any(hint in self.url for hint in ("/api/", "/graphql", "/rpc", "/trpc"))
        )


def read(path: Path) -> list[Exchange]:
    """Loads an events.jsonl file, skipping any partially written trailing line."""
    exchanges = []
    for line in path.read_text().splitlines():
        try:
            exchanges.append(Exchange.from_json(json.loads(line)))
        except (ValueError, KeyError):
            continue
    return exchanges
