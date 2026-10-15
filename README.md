# http-tunnel

[![Release](https://img.shields.io/github/v/release/jiangood/http-tunnel)](https://github.com/jiangood/http-tunnel/releases)
[![Build](https://img.shields.io/github/actions/workflow/status/jiangood/http-tunnel/rust.yml?branch=main)](https://github.com/jiangood/http-tunnel/actions)
[![License](https://img.shields.io/github/license/jiangood/http-tunnel)](./LICENSE)

**http-tunnel** is a Rust-based HTTP reverse tunnel for exposing services behind NAT through a public server. Forked from [rathole](https://github.com/rathole-org/rathole), it routes HTTP traffic by `Host` and lets clients manage their own routes.

## Features

- Multiple domains and wildcard routes on one public HTTP port
- WebSocket and protocol upgrade support
- No server-side tunnel configuration
- Automatic reconnect and pooled data channels
- Optional runtime client API

## Quick start

Download a [release](https://github.com/jiangood/http-tunnel/releases) or use the [Docker image](https://github.com/jiangood/http-tunnel/pkgs/container/http-tunnel).

Start the public server with a strong shared token:

```sh
./http-tunnel server --server-port 2333 --http-port 80 --token 'replace-with-a-long-random-secret'
```

Create `client.toml` on the machine running the local service:

```toml
remote = "myserver.com:2333"
name = "home"
token = "replace-with-a-long-random-secret"

[[tunnels]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
```

Start the client and point `nas.example.com` to the public server:

```sh
./http-tunnel --config client.toml
```

The server must accept client connections on TCP `2333` and visitor traffic on TCP `80`. The client config defaults to `client.toml`; command-line options override file values. `remote`, `name`, and `token` also support the `HTTP_TUNNEL_REMOTE`, `HTTP_TUNNEL_NAME`, and `HTTP_TUNNEL_TOKEN` environment variables.

## Configuration

Define one `[[tunnels]]` entry per route:

```toml
remote = "myserver.com:2333"
name = "home"
token = "replace-with-a-long-random-secret"
api_port = 8610
heartbeat_timeout = 40
retry_interval = 1
nodelay = true

[[tunnels]]
name = "app"
domain = "app.example.com"
local_addr = "127.0.0.1:8080"
```

Each tunnel requires a unique client-local `name`, a routing `domain`, and a client-reachable `local_addr`. `api_port = 0` disables the administration API; `heartbeat_timeout = 0` disables heartbeat timeout. `nodelay` defaults to `true`.

## Routing

- Exact domains take precedence over wildcards. `*.example.com` matches subdomains, not the apex domain.
- Host matching is case-insensitive; ports and trailing dots are ignored.
- Routing is selected from the first request on each TCP connection; keep-alive connections cannot switch routes.
- Unknown routes return `404`; failure to obtain a data channel within 10 seconds returns `504`.
- Multiple `Host` headers and malformed, oversized, or slow headers are rejected.

## Client API

The optional administration API listens on port `8610` by default. Requests require `Authorization: Bearer <token>`.

| Method | Endpoint | Purpose |
| --- | --- | --- |
| GET | `/api/status` | Client status |
| GET | `/api/tunnels` | List routes |
| PUT | `/api/tunnels/{name}` | Create or replace a route |
| DELETE | `/api/tunnels/{name}` | Delete a route |

For `PUT`, send JSON with `domain` and `local_addr` fields. The API binds to `0.0.0.0`; do not expose it directly to the Internet. Restrict access with a firewall or private network, or use a TLS-terminating reverse proxy.

## Docker

```sh
docker run -d --name http-tunnel --restart unless-stopped \
  -p 2333:2333 -p 80:80 \
  -e HTTP_TUNNEL_TOKEN='replace-with-a-long-random-secret' \
  ghcr.io/jiangood/http-tunnel:latest server
```

Run the client with its config mounted read-only or read-write as required (the API writes changes back to the file):

```sh
docker run -d --name http-tunnel-client --restart unless-stopped \
  -v "$PWD:/app" -w /app \
  ghcr.io/jiangood/http-tunnel:latest --config /app/client.toml
```

## Operations

Set `RUST_LOG=error|warn|info|debug|trace` to control logging; the default is `info`. TCP_NODELAY is enabled by default. Forwarding uses 64 KiB copy buffers on both ends.

Run the throughput benchmark with `cargo bench --bench throughput`. See [Out of Scope](./docs/out-of-scope.md) for intentional limitations.

## License

[Apache-2.0](./LICENSE)
