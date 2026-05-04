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
      let attempts = 0;
      const interval = setInterval(() => {
        attempts += 1;
        if (window.__agentMcpBDeepInstalled) {
          clearInterval(interval);
          setTimeout(() => {
            document.getElementById('go').click();
          }, 150);
          return;
        }
        if (attempts >= 50) {
          clearInterval(interval);
        }
      }, 100);
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

    def do_GET(self):
        if self.path == "/":
            body = INDEX_HTML.encode("utf-8")
            self.send_response(200)
            self.send_header("content-type", "text/html; charset=utf-8")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
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

        self._write_json(404, {"error": "not_found"})

    def do_POST(self):
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
    server = HTTPServer(("127.0.0.1", port), Handler)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
