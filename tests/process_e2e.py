#!/usr/bin/env python3
"""Process-level E2E smoke test for the hts, htc and htn executables.

Starts real hts/htc/htn processes and local HTTP backends, then checks
server-side proxies, client tunnels, the X-HTTP-TUNNEL-TOKEN gate, an
environment-only client, and a route-mirroring node chain.
Only Python's standard library is required.
"""
import base64
import json
import os
import shutil
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
NODE_TOKEN = "e2e-node-token"
SERVER_AUTH = "server-proxy-secret"
CLIENT_AUTH = "client-tunnel-secret"


class BackendHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        payload = json.dumps({
            "authorization": self.headers.get("Authorization"),
            "tunnel_token": self.headers.get("X-HTTP-TUNNEL-TOKEN"),
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


def check_web_management(port, read_only=False):
    base = f"http://127.0.0.1:{port}/"
    try:
        urllib.request.urlopen(base, timeout=3)
        raise AssertionError("web UI should require authentication")
    except urllib.error.HTTPError as exc:
        assert exc.code == 401, f"expected 401 without credentials, got {exc.code}"

    credentials = base64.b64encode(f"htc:{TOKEN}".encode()).decode()
    req = urllib.request.Request(base, headers={"Authorization": f"Basic {credentials}"})
    with urllib.request.urlopen(req, timeout=3) as response:
        page = response.read().decode()
        assert response.status == 200
        assert "http-tunnel 客户端管理" in page
        if read_only:
            assert "HTC_TUNNELS" in page, "the page must explain that the environment manages the tunnels"
            assert "<form" not in page.lower(), "the tunnel form must be hidden"
        else:
            assert "<form" in page
        assert "<script" not in page.lower()
        assert "<style" not in page.lower()

    api_req = urllib.request.Request(
        base + "api/status", headers={"Authorization": f"Bearer {TOKEN}"}
    )
    with urllib.request.urlopen(api_req, timeout=3) as response:
        assert response.status == 200
    print("PASS web management: Basic auth, plain HTML page, and existing Bearer API")


def request(port, host, token=None):
    headers = {"Host": host}
    if token is not None:
        headers["X-HTTP-TUNNEL-TOKEN"] = token
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/e2e",
        headers=headers,
    )
    with urllib.request.urlopen(req, timeout=5) as response:
        return response.status, json.loads(response.read())


def wait_for_tunnel(port, process, host, token, timeout=20):
    deadline = time.monotonic() + timeout
    last_error = None
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"htc exited early with code {process.returncode}")
        try:
            status, body = request(port, host, token)
            if status == 200:
                return body
        except (OSError, urllib.error.URLError, TimeoutError) as exc:
            last_error = exc
            time.sleep(0.2)
    raise TimeoutError(f"the tunnel {host} did not become ready: {last_error}")


def wait_for_status(port, host, expected, token=None, timeout=20):
    """Poll until the ingress answers `host` with `expected` (e.g. 404 once a
    mirrored route is withdrawn), instead of hanging on a stale route."""
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            status, _ = request(port, host, token)
        except urllib.error.HTTPError as exc:
            status = exc.code
        except (OSError, urllib.error.URLError, TimeoutError) as exc:
            last = exc
            time.sleep(0.2)
            continue
        if status == expected:
            return
        last = f"HTTP {status}"
        time.sleep(0.2)
    raise TimeoutError(f"{host} did not answer {expected} in time: {last}")


def cleanup(work):
    # Windows may keep a just-terminated process's working directory locked for a
    # short while. Retry, ignoring errors, so a failed cleanup never masks a pass.
    for _ in range(10):
        if not work.exists():
            return
        shutil.rmtree(work, ignore_errors=True)
        time.sleep(0.2)


def main():
    suffix = ".exe" if os.name == "nt" else ""
    hts = ROOT / "target" / "debug" / f"hts{suffix}"
    htc = ROOT / "target" / "debug" / f"htc{suffix}"
    htn = ROOT / "target" / "debug" / f"htn{suffix}"
    if not hts.is_file() or not htc.is_file() or not htn.is_file():
        raise SystemExit(
            "Missing target/debug/hts, htc or htn; run cargo build --all-features first"
        )

    server_backend = ThreadingHTTPServer(("127.0.0.1", 0), BackendHandler)
    client_backend = ThreadingHTTPServer(("127.0.0.1", 0), BackendHandler)
    for server in (server_backend, client_backend):
        threading.Thread(target=server.serve_forever, daemon=True).start()

    server_port, web_port = free_port(), free_port()
    while web_port == server_port:
        web_port = free_port()

    processes = []
    work = Path(tempfile.mkdtemp(prefix="http-tunnel-e2e-"))
    work_dirs = [work]
    try:
        (work / "hts.toml").write_text(f"""server_port = {server_port}
token = "{TOKEN}"
server_host = "127.0.0.1"
bind_host = "127.0.0.1"

[[proxies]]
name = "server-backend"
domain = "server.e2e.test"
local_addr = "127.0.0.1:{server_backend.server_port}"
auth_header = "{SERVER_AUTH}"
""")
        (work / "htc.toml").write_text(f"""server = "ws://127.0.0.1:{server_port}"
name = "e2e-client"
token = "{TOKEN}"
web_port = {web_port}
web_host = "127.0.0.1"
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
        wait_for_port(server_port, server, "hts HTTP listener")

        client = subprocess.Popen(
            [str(htc)], cwd=work, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True,
        )
        processes.append(client)
        wait_for_port(web_port, client, "htc web management listener")
        check_web_management(web_port)
        deadline = time.monotonic() + 20
        last_error = None
        while time.monotonic() < deadline:
            if client.poll() is not None:
                raise RuntimeError(f"htc exited early with code {client.returncode}")
            try:
                status, body = request(server_port, "client.e2e.test", CLIENT_AUTH)
                if status == 200:
                    break
            except (OSError, urllib.error.URLError, TimeoutError) as exc:
                last_error = exc
                time.sleep(0.2)
        else:
            raise TimeoutError(f"client tunnel did not become ready: {last_error}")

        for host, secret in (
            ("server.e2e.test", SERVER_AUTH),
            ("client.e2e.test", CLIENT_AUTH),
        ):
            status, body = request(server_port, host, secret)
            assert status == 200, f"{host}: unexpected HTTP status {status}"
            assert body["tunnel_token"] is None, (
                f"{host}: the token must be stripped before the backend, "
                f"got {body['tunnel_token']!r}"
            )
            print(f"PASS {host}: real hts/htc process gated the request by X-HTTP-TUNNEL-TOKEN")

        for host in ("server.e2e.test", "client.e2e.test"):
            try:
                request(server_port, host)
            except urllib.error.HTTPError as exc:
                assert exc.code == 401, f"{host}: expected 401 without the token, got {exc.code}"
                body = exc.read().decode()
                assert "Missing X-HTTP-TUNNEL-TOKEN header" in body, (
                    f"{host}: unexpected 401 body {body!r}"
                )
            else:
                raise AssertionError(f"{host}: a request without the token must get 401")
            print(f"PASS {host}: a request without the token is answered with 401 (missing)")

            try:
                request(server_port, host, "wrong-secret")
            except urllib.error.HTTPError as exc:
                assert exc.code == 401, f"{host}: expected 401 for a wrong token, got {exc.code}"
                body = exc.read().decode()
                assert "Invalid X-HTTP-TUNNEL-TOKEN header" in body, (
                    f"{host}: unexpected 401 body {body!r}"
                )
            else:
                raise AssertionError(f"{host}: a wrong token must get 401")
            print(f"PASS {host}: a wrong token is answered with 401 (invalid)")

        # Phase 2: the same server, a client configured only by HTC_* variables
        # and with no htc.toml file.
        client.terminate()
        client.wait(timeout=5)
        client.stdout.close()
        processes.remove(client)

        env_work = Path(tempfile.mkdtemp(prefix="http-tunnel-e2e-env-"))
        work_dirs.append(env_work)
        env_web_port = free_port()
        env = os.environ.copy()
        env.update(
            {
                "HTC_SERVER": f"ws://127.0.0.1:{server_port}",
                "HTC_NAME": "e2e-env-client",
                "HTC_TOKEN": TOKEN,
                "HTC_WEB_PORT": str(env_web_port),
                "HTC_WEB_HOST": "127.0.0.1",
                "HTC_RETRY_INTERVAL": "1",
                "HTC_TUNNELS": (
                    f'[{{name="client-backend",domain="env-client.e2e.test",'
                    f'local_addr="127.0.0.1:{client_backend.server_port}",'
                    f'auth_header="{CLIENT_AUTH}"}}]'
                ),
            }
        )

        client = subprocess.Popen(
            [str(htc)], cwd=env_work, env=env, stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT, text=True,
        )
        processes.append(client)
        wait_for_port(env_web_port, client, "htc environment-only web listener")
        assert not (env_work / "htc.toml").exists(), (
            "an environment-only client must not write htc.toml"
        )
        check_web_management(env_web_port, read_only=True)
        body = wait_for_tunnel(server_port, client, "env-client.e2e.test", CLIENT_AUTH)
        assert body["tunnel_token"] is None, (
            "the token must be stripped before the backend, "
            f"got {body['tunnel_token']!r}"
        )

        # The tunnels of the environment are read-only through the API
        api_req = urllib.request.Request(
            f"http://127.0.0.1:{env_web_port}/api/tunnels/extra",
            method="PUT",
            data=b'{"domain":"extra.e2e.test","local_addr":"127.0.0.1:1"}',
            headers={
                "Authorization": f"Bearer {TOKEN}",
                "Content-Type": "application/json",
            },
        )
        try:
            urllib.request.urlopen(api_req, timeout=3)
        except urllib.error.HTTPError as exc:
            assert exc.code == 409, (
                f"an environment-managed tunnel must be read-only, got {exc.code}"
            )
        else:
            raise AssertionError("an environment-managed tunnel must be read-only")
        print(
            "PASS HTC_TUNNELS: an env-only client routes its tunnel, writes no "
            "htc.toml, and rejects API changes with 409"
        )

        # Phase 3: a real htn relay mirrors a real htc leaf to the ingress.
        node_port = free_port()
        while node_port in (server_port, web_port, env_web_port):
            node_port = free_port()
        node_work = Path(tempfile.mkdtemp(prefix="http-tunnel-e2e-node-"))
        chain_work = Path(tempfile.mkdtemp(prefix="http-tunnel-e2e-chain-"))
        work_dirs.extend([node_work, chain_work])
        (node_work / "htn.toml").write_text(f"""name = "e2e-relay"
server_port = {node_port}
server_host = "127.0.0.1"
bind_host = "127.0.0.1"
token = "{NODE_TOKEN}"

[upstream]
server = "ws://127.0.0.1:{server_port}"
token = "{TOKEN}"
mirror = true
""")
        (chain_work / "htc.toml").write_text(f"""server = "ws://127.0.0.1:{node_port}"
name = "e2e-chain-leaf"
token = "{NODE_TOKEN}"
web_port = 0
retry_interval = 1

[[tunnels]]
name = "chain-backend"
domain = "chain.e2e.test"
local_addr = "127.0.0.1:{client_backend.server_port}"
auth_header = "{CLIENT_AUTH}"
""")

        node = subprocess.Popen(
            [str(htn)], cwd=node_work, stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT, text=True,
        )
        processes.append(node)
        wait_for_port(node_port, node, "htn listener")

        leaf = subprocess.Popen(
            [str(htc)], cwd=chain_work, stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT, text=True,
        )
        processes.append(leaf)
        body = wait_for_tunnel(server_port, leaf, "chain.e2e.test", CLIENT_AUTH, timeout=25)
        assert body["tunnel_token"] is None, (
            "the token must be stripped before the backend across the chain, "
            f"got {body['tunnel_token']!r}"
        )
        print(
            "PASS chain.e2e.test: a real htn mirror relays a real htc leaf, and "
            "the token is stripped"
        )

        leaf.terminate()
        leaf.wait(timeout=5)
        leaf.stdout.close()
        processes.remove(leaf)
        wait_for_status(server_port, "chain.e2e.test", 404, timeout=20)
        print(
            "PASS chain.e2e.test: stopping the leaf withdraws the mirrored route "
            "and the ingress answers 404"
        )

        # The node logs the withdrawal itself; check it before the shutdown log
        # noise, so the test proves the upstream claim was dropped rather than
        # only that the node happens to answer 404.
        node.terminate()
        node_output, _ = node.communicate(timeout=5)
        processes.remove(node)
        assert "Withdrawing the mirrored route" in node_output, (
            "the node did not log the withdrawal of the mirrored route:\n"
            f"{node_output[-2000:]}"
        )
        print("PASS chain.e2e.test: the node withdrew the mirrored route upstream")

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
        for directory in reversed(work_dirs):
            cleanup(directory)


if __name__ == "__main__":
    main()
