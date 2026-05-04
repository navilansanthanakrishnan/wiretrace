#!/usr/bin/env python3
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import sys
import urllib.parse


INDEX_HTML = """<!doctype html>
<html>
<head>
  <meta charset="utf-8" />
  <title>Workflow Fixture</title>
</head>
<body>
  <h1>Workflow Fixture</h1>
  <button id="go">Send workflow request</button>
  <script>
    function autoTrigger(buttonId) {
      let attempts = 0;
      const interval = setInterval(() => {
        attempts += 1;
        if (window.__agentMcpBDeepInstalled) {
          clearInterval(interval);
          setTimeout(() => {
            document.getElementById(buttonId).click();
          }, 150);
          return;
        }
        if (attempts >= 15) {
          clearInterval(interval);
          setTimeout(() => {
            document.getElementById(buttonId).click();
          }, 150);
        }
      }, 100);
    }

    async function sendWorkflowRequest() {
      await fetch('/api/config', { headers: { 'accept': 'application/json' } });
      await fetch('/api/submit', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ action: 'ship', target: 'message', count: 1 })
      });
    }

    document.getElementById('go').addEventListener('click', () => {
      sendWorkflowRequest();
    });

    window.addEventListener('load', () => {
      autoTrigger('go');
    });
  </script>
</body>
</html>
"""

AUTH_HTML = """<!doctype html>
<html>
<head>
  <meta charset="utf-8" />
  <title>Workflow Auth Fixture</title>
</head>
<body>
  <h1>Workflow Auth Fixture</h1>
  <button id="login">Login and load private data</button>
  <script>
    function autoTrigger(buttonId) {
      let attempts = 0;
      const interval = setInterval(() => {
        attempts += 1;
        if (window.__agentMcpBDeepInstalled) {
          clearInterval(interval);
          setTimeout(() => {
            document.getElementById(buttonId).click();
          }, 150);
          return;
        }
        if (attempts >= 15) {
          clearInterval(interval);
          setTimeout(() => {
            document.getElementById(buttonId).click();
          }, 150);
        }
      }, 100);
    }

    async function loginAndLoad() {
      await fetch('/auth/login', {
        method: 'POST',
        credentials: 'include',
        headers: {
          'content-type': 'application/json',
          'authorization': 'Bearer fixture-login'
        },
        body: JSON.stringify({ username: 'demo-user' })
      });

      await fetch('/api/private', {
        method: 'GET',
        credentials: 'include',
        headers: {
          'accept': 'application/json',
          'x-api-client': 'workflow-auth-fixture'
        }
      });
    }

    document.getElementById('login').addEventListener('click', () => {
      loginAndLoad();
    });

    window.addEventListener('load', () => {
      autoTrigger('login');
    });
  </script>
</body>
</html>
"""


class Handler(BaseHTTPRequestHandler):
    def _write_json(self, status, payload):
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _write_html(self, html):
        body = html.encode("utf-8")
        self.send_response(200)
        self.send_header("content-type", "text/html; charset=utf-8")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/":
            self._write_html(INDEX_HTML)
            return

        if self.path == "/auth":
            self._write_html(AUTH_HTML)
            return

        if self.path.startswith("/api/config"):
            self._write_json(
                200,
                {
                    "product": "workflow-fixture",
                    "features": ["record", "map", "automate"],
                    "transport": "http",
                },
            )
            return

        if self.path.startswith("/api/private"):
            cookie_header = self.headers.get("cookie", "")
            if "sid=workflow-session" not in cookie_header:
                self._write_json(401, {"error": "missing_session"})
                return
            self._write_json(
                200,
                {
                    "ok": True,
                    "user": "demo-user",
                    "permissions": ["read", "write"],
                },
            )
            return

        self._write_json(404, {"error": "not_found"})

    def do_POST(self):
        if self.path.startswith("/auth/login"):
            length = int(self.headers.get("content-length", "0"))
            body = self.rfile.read(length).decode("utf-8") if length else "{}"
            parsed = json.loads(body or "{}")
            response = json.dumps(
                {
                    "ok": True,
                    "user": parsed.get("username", "unknown"),
                    "session": "workflow-session",
                }
            ).encode("utf-8")
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("set-cookie", "sid=workflow-session; Path=/; HttpOnly; SameSite=Lax")
            self.send_header("content-length", str(len(response)))
            self.end_headers()
            self.wfile.write(response)
            return

        if self.path.startswith("/api/submit"):
            length = int(self.headers.get("content-length", "0"))
            body = self.rfile.read(length).decode("utf-8") if length else "{}"
            parsed = json.loads(body or "{}")
            self._write_json(
                200,
                {
                    "ok": True,
                    "received": parsed,
                    "route": self.path,
                },
            )
            return

        self._write_json(404, {"error": "not_found"})

    def log_message(self, format, *args):
        return


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8012
    host = sys.argv[2] if len(sys.argv) > 2 else "127.0.0.1"
    server = HTTPServer((host, port), Handler)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
