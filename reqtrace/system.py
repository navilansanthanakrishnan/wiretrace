"""macOS glue: system proxy settings and CA trust.

Native apps have no idea reqtrace exists; they reach the proxy because the OS
tells them to. That is the whole of what this module does.
"""

from __future__ import annotations

import re
import subprocess
from pathlib import Path


def run(*args: str) -> str:
    result = subprocess.run(args, capture_output=True, text=True)
    if result.returncode:
        raise RuntimeError(f"{args[0]} failed: {result.stderr.strip() or result.stdout.strip()}")
    return result.stdout


def active_service() -> str:
    """The network service carrying the default route, e.g. `Wi-Fi`."""
    device = re.search(r"interface: (\S+)", run("route", "-n", "get", "default"))
    if not device:
        raise RuntimeError("no default route; is this machine online?")
    order = run("networksetup", "-listnetworkserviceorder")
    match = re.search(rf"\(\d+\) (.+)\n.*Device: {device.group(1)}\)", order)
    if not match:
        raise RuntimeError(f"no network service for interface {device.group(1)}")
    return match.group(1)


def set_proxy(service: str, host: str, port: int) -> None:
    for kind in ("-setwebproxy", "-setsecurewebproxy"):
        run("networksetup", kind, service, host, str(port))


def clear_proxy(service: str) -> None:
    for kind in ("-setwebproxystate", "-setsecurewebproxystate"):
        subprocess.run(["networksetup", kind, service, "off"], capture_output=True)


def ca_trusted() -> bool:
    trust = subprocess.run(["security", "dump-trust-settings"], capture_output=True, text=True)
    return "reqtrace local CA" in trust.stdout


def trust_ca(cert: Path) -> None:
    """Adds the CA to the login keychain. Prompts for the user's password."""
    keychain = Path.home() / "Library/Keychains/login.keychain-db"
    run("security", "add-trusted-cert", "-r", "trustRoot", "-p", "ssl", "-k", str(keychain), str(cert))
