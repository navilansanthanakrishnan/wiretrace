"""End-to-end: run a real capture against a local API and check what we infer.

Uses plain HTTP through the proxy so the test needs no certificate trust.
"""

import json
import os
import socket
import ssl
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import httpx
import pytest

os.environ["WIRETRACE_HOME"] = tempfile.mkdtemp(prefix="wiretrace-test-")

from wiretrace import session as sessions  # noqa: E402
from wiretrace.api import infer, looks_like_id, templatize  # noqa: E402
from wiretrace.events import Exchange  # noqa: E402
from wiretrace.export import export, openapi  # noqa: E402


class Fixture(BaseHTTPRequestHandler):
    """A tiny API: one collection read and one nested write."""

    def respond(self, payload):
        body = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.respond({"channels": [{"id": "84121299", "name": "general"}]})

    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        sent = json.loads(self.rfile.read(length) or b"{}")
        self.respond({"id": "99", "content": sent.get("content"), "ok": True})

    def log_message(self, *_):
        pass


@pytest.fixture(scope="module")
def api_server():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()


def test_templatize_finds_identifiers():
    assert templatize("/api/v9/channels/84121299/messages") == (
        "/api/v9/channels/{channel_id}/messages",
        ["channel_id"],
    )
    assert templatize("/api/health") == ("/api/health", [])
    assert not looks_like_id("messages")


def test_capture_infers_and_exports(api_server, tmp_path):
    host = api_server.removeprefix("http://")
    session = sessions.start(host, mode="proxy", system_proxy=False, port=free_port())
    try:
        wait_for_proxy(session.port)
        proxy = f"http://127.0.0.1:{session.port}"
        with httpx.Client(proxy=proxy, timeout=10) as client:
            client.get(f"{api_server}/api/v9/channels", headers={"authorization": "Bearer secret"})
            for channel in ("84121299", "84121300"):
                client.post(
                    f"{api_server}/api/v9/channels/{channel}/messages",
                    json={"content": "hello"},
                    headers={"authorization": "Bearer secret"},
                )
        time.sleep(0.5)
        api = session.stop()
    finally:
        session.stop()

    # Two distinct call shapes, not three requests.
    assert len(api.endpoints) == 2
    post = next(e for e in api.endpoints if e.method == "POST")
    assert post.path == "/api/v9/channels/{channel_id}/messages"
    assert post.calls == 2
    assert post.body_schema["properties"]["content"] == {"type": "string"}
    assert post.response_schema["properties"]["ok"] == {"type": "boolean"}
    assert post.auth == ["authorization"]

    # The credential is usable but never part of the API description.
    assert "secret" not in json.dumps(api.to_dict())
    assert json.loads((session.dir / "credentials.json").read_text())[host]["authorization"]

    spec = openapi(api.endpoints, host)
    assert spec["paths"]["/api/v9/channels/{channel_id}/messages"]["post"]["operationId"] == post.id

    written = export(session, tmp_path / "out")
    assert {path.name for path in written} == {"api.json", "openapi.json", "server.py", "credentials.json"}
    # The export must outlive its session, so it carries its own credentials.
    assert (tmp_path / "out" / "credentials.json").stat().st_mode & 0o777 == 0o600
    compile((tmp_path / "out" / "server.py").read_text(), "server.py", "exec")


def test_https_interception_satisfies_strict_clients():
    """The leaf certificate must pass strict X.509 validation.

    Python 3.13 turns strict verification on by default, so a leaf without an
    authority key identifier fails here while curl and Node sail through — the
    kind of gap only a real TLS request finds. Needs network.
    """
    session = sessions.start("api.open-meteo.com", mode="proxy", system_proxy=False, port=free_port())
    try:
        response = httpx.get(
            "https://api.open-meteo.com/v1/forecast?latitude=52.5&longitude=13.4&current_weather=true",
            proxy=f"http://127.0.0.1:{session.port}",
            verify=ssl.create_default_context(cafile=str(sessions.CERTS / "ca-cert.pem")),
            timeout=20,
        )
    except httpx.ConnectError as error:
        if "CERTIFICATE_VERIFY_FAILED" in str(error):
            raise
        pytest.skip(f"no network: {error}")
    finally:
        api = session.stop()

    assert response.status_code == 200
    endpoint = api.get("get_forecast")
    # Rebuilt from the port-stripped host, so one endpoint however the client spells it.
    assert endpoint.host == "api.open-meteo.com"
    assert "latitude" in endpoint.query_params


def test_secrets_never_reach_the_api_description():
    def exchange(url, headers):
        return Exchange(0, "proxy", "GET", url, headers, None, 200,
                        {"content-type": "application/json"}, "{}", 1, None)

    api = infer([exchange("https://x.com/data?api_key=SEKRIT&city=berlin",
                          {"authorization": "Bearer TOKEN", "user-agent": "curl/8"})])
    described = json.dumps(api.to_dict())
    assert "SEKRIT" not in described and "TOKEN" not in described
    assert api.credentials["x.com"] == {"?api_key": "SEKRIT", "authorization": "Bearer TOKEN"}
    endpoint = api.endpoints[0]
    assert endpoint.query_params == {"api_key": "<captured>", "city": "berlin"}
    # Non-secret client headers are kept, because private APIs often demand them.
    assert endpoint.headers == {"user-agent": "curl/8"}


def test_har_import_matches_a_live_capture(tmp_path):
    """A HAR is the zero-setup path in, so it must infer the same API a capture would."""
    har = {"log": {"entries": [
        {"startedDateTime": "2026-08-20T10:00:00.000Z", "time": 12,
         "request": {"method": "POST", "url": "https://x.com/api/channels/84121299/messages",
                     "headers": [{"name": "Authorization", "value": "Bearer SEKRIT"},
                                 {"name": "User-Agent", "value": "Firefox"}],
                     "postData": {"text": '{"content": "hi"}'}},
         "response": {"status": 200, "headers": [{"name": "Content-Type", "value": "application/json"}],
                      "content": {"text": '{"id": "1", "ok": true}'}}},
        {"startedDateTime": "2026-08-20T10:00:01.000Z", "time": 9,
         "request": {"method": "POST", "url": "https://x.com/api/channels/84121300/messages",
                     "headers": [{"name": "Authorization", "value": "Bearer SEKRIT"}],
                     "postData": {"text": '{"content": "there"}'}},
         "response": {"status": 200, "headers": [{"name": "Content-Type", "value": "application/json"}],
                      "content": {"text": '{"id": "2", "ok": true}'}}},
    ]}}
    path = tmp_path / "capture.har"
    path.write_text(json.dumps(har))

    session = sessions.ingest(path)
    api = session.api()

    assert len(api.endpoints) == 1
    endpoint = api.endpoints[0]
    assert endpoint.method == "POST"
    assert endpoint.path == "/api/channels/{channel_id}/messages"
    assert endpoint.calls == 2
    assert endpoint.body_schema["properties"]["content"] == {"type": "string"}
    assert endpoint.headers == {"user-agent": "Firefox"}
    # The same credential rule applies to imported traffic as to captured traffic.
    assert endpoint.auth == ["authorization"]
    assert "SEKRIT" not in json.dumps(api.to_dict())
    assert json.loads((session.dir / "credentials.json").read_text())["x.com"]["authorization"]


def test_is_api_skips_page_furniture():
    def exchange(url, content_type="text/html", method="GET"):
        return Exchange(0, "proxy", method, url, {}, None, 200, {"content-type": content_type}, None, 1, None)

    assert not exchange("https://x.com/app.js").is_api()
    assert not exchange("https://x.com/").is_api()
    assert exchange("https://x.com/data", "application/json").is_api()
    assert exchange("https://x.com/save", method="POST").is_api()
    assert infer([exchange("https://x.com/app.js")]).endpoints == []


def free_port():
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def wait_for_proxy(port, attempts=50):
    for _ in range(attempts):
        try:
            httpx.get("http://127.0.0.1:%d" % port, timeout=1)
            return
        except httpx.HTTPError:
            time.sleep(0.1)
    raise AssertionError("proxy never came up")
