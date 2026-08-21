"""A capture session: start observing, stop, keep the result on disk.

Layout of `~/.reqtrace/sessions/<id>/`:

    session.json      what was captured and how
    events.jsonl      raw exchanges, written live by the capture binary
    api.json          the inferred API (written at stop)
    credentials.json  auth headers observed per host, mode 0600
"""

from __future__ import annotations

import json
import os
import socket
import shutil
import signal
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path

from . import cdp, system
from .api import Api, infer
from .events import read

HOME = Path(os.environ.get("REQTRACE_HOME", Path.home() / ".reqtrace"))
SESSIONS, CERTS, PROFILES = HOME / "sessions", HOME / "certs", HOME / "profiles"


def capture_binary() -> Path:
    """The Rust capture binary: an override, a release build, or one on PATH."""
    override = os.environ.get("REQTRACE_CAPTURE_BIN")
    candidates = [
        Path(override) if override else None,
        Path(__file__).parent.parent / "capture/target/release/reqtrace-capture",
        Path(shutil.which("reqtrace-capture") or "/nonexistent"),
    ]
    for candidate in candidates:
        if candidate and candidate.exists():
            return candidate
    raise RuntimeError("reqtrace-capture not built; run: cargo build --release --manifest-path capture/Cargo.toml")


@dataclass
class Session:
    id: str
    dir: Path
    target: str
    mode: str
    port: int = 0
    pid: int | None = None
    service: str | None = None
    stopped: bool = False

    @property
    def events_path(self) -> Path:
        return self.dir / "events.jsonl"

    @property
    def recording(self) -> bool:
        if self.stopped or not self.pid:
            return False
        try:
            os.kill(self.pid, 0)
            return True
        except OSError:
            return False

    def save(self) -> "Session":
        record = {k: str(v) if isinstance(v, Path) else v for k, v in vars(self).items()}
        (self.dir / "session.json").write_text(json.dumps(record, indent=2))
        return self

    @classmethod
    def load(cls, session_id: str) -> "Session":
        path = SESSIONS / session_id / "session.json"
        if not path.exists():
            raise RuntimeError(f"no session {session_id}")
        record = json.loads(path.read_text())
        return cls(**{**record, "dir": Path(record["dir"])})

    def navigate(self, url: str) -> None:
        """Points the captured tab at a URL.

        It must be the *same* tab the capture attached to — a new tab is a new
        CDP target and nothing in it would be recorded.
        """
        self.require_browser()
        cdp.navigate(self.port, url)

    def evaluate(self, expression: str) -> str:
        """Runs JavaScript in the captured tab: click, fill a field, scroll."""
        self.require_browser()
        return cdp.evaluate(self.port, expression)

    def require_browser(self) -> None:
        if self.mode != "browser":
            raise RuntimeError(f"session {self.id} is a {self.mode} capture; there is no page to drive")
        if not self.recording:
            raise RuntimeError(f"session {self.id} is not recording")

    def await_ready(self, child: subprocess.Popen) -> None:
        """Blocks until the capture can actually see traffic.

        Without this the first request races the proxy's bind, and a capture
        that started too late looks exactly like an app that was never used.
        """
        for _ in range(100):
            if child.poll() is not None:
                raise RuntimeError(f"capture exited immediately:\n{self.log()}")
            if listening(self.port):
                return
            time.sleep(0.1)
        raise RuntimeError(f"capture never opened port {self.port}:\n{self.log()}")

    def seen(self) -> int:
        """Raw exchanges captured, whether or not they looked like API calls."""
        return len(read(self.events_path)) if self.events_path.exists() else 0

    def log(self) -> str:
        path = self.dir / "capture.log"
        return path.read_text()[-2000:] if path.exists() else "(no capture log)"

    def api(self) -> Api:
        """The inferred API, computed on first read and cached on disk."""
        cached = self.dir / "api.json"
        if cached.exists():
            return Api.from_dict(json.loads(cached.read_text()))
        return self.reinfer()

    def reinfer(self) -> Api:
        api = infer(read(self.events_path)) if self.events_path.exists() else Api()
        (self.dir / "api.json").write_text(json.dumps(api.to_dict(), indent=2))
        write_private(self.dir / "credentials.json", api.credentials)
        return api

    def stop(self) -> Api:
        """Ends capture and materializes the inferred API."""
        if self.pid and self.recording:
            os.kill(self.pid, signal.SIGINT)
            for _ in range(50):
                if not self.recording:
                    break
                time.sleep(0.1)
        if self.service:
            system.clear_proxy(self.service)
        self.stopped = True
        self.save()
        return self.reinfer()


def start(
    target: str,
    mode: str = "browser",
    hosts: list[str] | None = None,
    port: int = 0,
    system_proxy: bool = True,
    headless: bool = False,
) -> Session:
    """Begins a capture.

    `browser` launches a managed Chrome at `target` and attributes requests to
    clicks. `proxy` intercepts anything pointed at the local proxy — the macOS
    system proxy by default. With `system_proxy` off nothing on the machine
    changes and you route traffic through the proxy yourself.
    """
    if active := current():
        raise RuntimeError(f"session {active.id} is already recording; stop it first")

    session_id = f"s{int(time.time())}"
    directory = SESSIONS / session_id
    directory.mkdir(parents=True, mode=0o700)
    port = port or (9222 if mode == "browser" else 8787)
    session = Session(id=session_id, dir=directory, target=target, mode=mode, port=port)

    if mode == "browser":
        claim(port)
        command = [
            str(capture_binary()), "browser",
            "--open", target,
            "--profile", str(PROFILES / session_id),
            "--port", str(port),
        ] + (["--headless"] if headless else [])
    elif mode == "proxy":
        claim(port)
        CERTS.mkdir(parents=True, exist_ok=True)
        command = [str(capture_binary()), "proxy", "--listen", f"127.0.0.1:{port}", "--cert-dir", str(CERTS)]
        for host in dict.fromkeys([target, *(hosts or [])]):
            command += ["--host", hostname(host)]
    else:
        raise RuntimeError(f"unknown mode {mode!r}; use browser or proxy")

    # Raw exchanges hold cookies and tokens verbatim — the least redacted thing
    # reqtrace writes, so it gets the tightest mode.
    session.events_path.touch(mode=0o600)
    with session.events_path.open("w") as events, (directory / "capture.log").open("w") as log:
        child = subprocess.Popen(command, stdout=events, stderr=log)
    session.pid = child.pid
    session.await_ready(child)

    if mode == "proxy" and system_proxy:
        session.service = system.active_service()
        system.set_proxy(session.service, "127.0.0.1", port)

    return session.save()


def listening(port: int) -> bool:
    with socket.socket() as probe:
        probe.settimeout(0.5)
        return probe.connect_ex(("127.0.0.1", port)) == 0


def claim(port: int) -> None:
    """Fails early and clearly when something else already owns the proxy port."""
    with socket.socket() as probe:
        probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            probe.bind(("127.0.0.1", port))
        except OSError as error:
            raise RuntimeError(f"port {port} is busy; start again with a different --port") from error


def hostname(target: str) -> str:
    """`https://example.com:443/path` -> `example.com`, so hosts filter as written."""
    return target.split("://")[-1].split("/")[0].split(":")[0]


def current() -> Session | None:
    """The session that is recording right now, if any."""
    return next((s for s in all_sessions() if s.recording), None)


def all_sessions() -> list[Session]:
    SESSIONS.mkdir(parents=True, exist_ok=True)
    sessions = []
    for path in sorted(SESSIONS.iterdir(), key=modified, reverse=True):
        try:
            sessions.append(Session.load(path.name))
        except (RuntimeError, TypeError, ValueError):
            continue
    return sessions


def remove(session_id: str) -> str:
    """Deletes a capture. They hold real cookies, so they should not pile up."""
    session = Session.load(session_id)
    if session.recording:
        raise RuntimeError(f"session {session_id} is still recording; stop it first")
    shutil.rmtree(session.dir)
    shutil.rmtree(PROFILES / session_id, ignore_errors=True)
    return session_id


def resolve(session_id: str | None) -> Session:
    """A session by id, defaulting to the most recent one."""
    if session_id:
        return Session.load(session_id)
    sessions = all_sessions()
    if not sessions:
        raise RuntimeError("no sessions yet; start a capture first")
    return sessions[0]


def modified(path: Path) -> float:
    record = path / "session.json"
    return record.stat().st_mtime if record.exists() else 0.0


def write_private(path: Path, payload: object) -> None:
    """Writes owner-only, and owner-only from the moment the file exists.

    Credentials live here so replay works without ever handing the secret to the
    agent; creating at the umask and chmod-ing after would leave a window.
    """
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(descriptor, "w") as handle:
        json.dump(payload, handle, indent=2)
    path.chmod(0o600)
