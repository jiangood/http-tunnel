#!/usr/bin/env python3
"""Process-level E2E smoke test for the hts and htc executables.

Starts real hts/htc processes and two local HTTP backends, then checks both
server-side proxies and client tunnels, including Authorization replacement.
Only Python's standard library is required.
"""
import json
import os
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TOKEN = "e2e-shared-token"
SERVER_AUTH = "server-proxy-secret"
CLIENT_AUTH = "client-tunnel-secret"


class BackendHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        payload = json.dumps({
            "authorization": self.headers.get("Authorization"),
            "path": self.path,
        }).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *_args):
        pass


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_for_port(port, process, label, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"{label} exited early with code {process.returncode}")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.3):
                return
        except OSError:
            time.sleep(0.1)
    raise TimeoutError(f"{label} did not listen on port {port} in {timeout}s")


def request(port, host):
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/e2e",
        headers={"Host": host, "Authorization": "Bearer incoming-wrong-value"},
    )
    with urllib.request.urlopen(req, timeout=5) as response:
        return response.status, json.loads(response.read())


def main():
    hts = ROOT / "target" / "debug" / "hts"
    htc = ROOT / "target" / "debug" / "htc"
    if not hts.is_file() or not htc.is_file():
        raise SystemExit("Missing target/debug/hts or htc; run cargo build --all-features first")

    server_backend = ThreadingHTTPServer(("127.0.0.1", 0), BackendHandler)
    client_backend = ThreadingHTTPServer(("127.0.0.1", 0), BackendHandler)
    for server in (server_backend, client_backend):
        threading.Thread(target=server.serve_forever, daemon=True).start()

    control_port, http_port = free_port(), free_port()
    while http_port == control_port:
        http_port = free_port()

    processes = []
    try:
        with tempfile.TemporaryDirectory(prefix="http-tunnel-e2e-") as tmp:
            work = Path(tmp)
            (work / "hts.toml").write_text(f"""server_port = {control_port}
http_port = {http_port}
token = "{TOKEN}"

[[proxies]]
name = "server-backend"
domain = "server.e2e.test"
local_addr = "127.0.0.1:{server_backend.server_port}"
auth_header = "{SERVER_AUTH}"
""")
            (work / "htc.toml").write_text(f"""remote = "127.0.0.1:{control_port}"
name = "e2e-client"
token = "{TOKEN}"
api_port = 0
retry_interval = 1

[[tunnels]]
name = "client-backend"
domain = "client.e2e.test"
local_addr = "127.0.0.1:{client_backend.server_port}"
auth_header = "{CLIENT_AUTH}"
""")

            server = subprocess.Popen(
                [str(hts)], cwd=work, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                text=True,
            )
            processes.append(server)
            wait_for_port(control_port, server, "hts control listener")
            wait_for_port(http_port, server, "hts HTTP listener")

            client = subprocess.Popen(
                [str(htc)], cwd=work, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                text=True,
            )
            processes.append(client)
            deadline = time.monotonic() + 20
            last_error = None
            while time.monotonic() < deadline:
                if client.poll() is not None:
                    raise RuntimeError(f"htc exited early with code {client.returncode}")
                try:
                    status, body = request(http_port, "client.e2e.test")
                    if status == 200:
                        break
                except (OSError, urllib.error.URLError, TimeoutError) as exc:
                    last_error = exc
                    time.sleep(0.2)
            else:
                raise TimeoutError(f"client tunnel did not become ready: {last_error}")

            for host, expected in (
                ("server.e2e.test", f"Bearer {SERVER_AUTH}"),
                ("client.e2e.test", f"Bearer {CLIENT_AUTH}"),
            ):
                status, body = request(http_port, host)
                assert status == 200, f"{host}: unexpected HTTP status {status}"
                assert body["authorization"] == expected, (
                    f"{host}: expected Authorization {expected!r}, "
                    f"got {body['authorization']!r}"
                )
                print(f"PASS {host}: real hts/htc process forwarded request and replaced Authorization")

    finally:
        for process in reversed(processes):
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
            if process.stdout:
                process.stdout.close()
        server_backend.shutdown()
        client_backend.shutdown()
        server_backend.server_close()
        client_backend.server_close()


if __name__ == "__main__":
    main()
