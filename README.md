# http-tunnel

[![GitHub release](https://img.shields.io/github/v/release/jiangood/http-tunnel)](https://github.com/jiangood/http-tunnel/releases)
[![Build](https://img.shields.io/github/actions/workflow/status/jiangood/http-tunnel/rust.yml?branch=main)](https://github.com/jiangood/http-tunnel/actions)
[![License](https://img.shields.io/github/license/jiangood/http-tunnel)](./LICENSE)

A Rust-based HTTP reverse tunnel for exposing services behind NAT through a server with a public IP. Forked from [rathole](https://github.com/rathole-org/rathole).

The server routes incoming HTTP connections by `Host`; clients own their tunnel configuration and register routes when connected. Traffic between the server and client travels over TCP.

## Features

- One public HTTP port for multiple domains, including wildcard domains.
- Transparent WebSocket and other protocol upgrades.
- Client-managed tunnels; no server configuration file.
- Reconnects automatically and maintains a pool of data channels.
- Runtime tunnel management through an optional client administration API.

## Quick start

Download a binary from [Releases](https://github.com/jiangood/http-tunnel/releases), or use the [Docker image](https://github.com/jiangood/http-tunnel/pkgs/container/http-tunnel).

### 1. Start the public server

Choose a strong shared token and keep it private:

```sh
./http-tunnel server --server-port 2333 --http-port 80 --token 'replace-with-a-long-random-secret'
```

The server needs TCP port `2333` reachable by clients and port `80` reachable by visitors. The token can also be set with `HTTP_TUNNEL_TOKEN`; the ports support `HTTP_TUNNEL_SERVER_PORT` and `HTTP_TUNNEL_HTTP_PORT`.

### 2. Configure the client

Create `client.toml` on the machine behind NAT:

```toml
remote = "myserver.com:2333"
name = "home-nas"
token = "replace-with-a-long-random-secret"

[[tunnels]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
```

Start the client:

```sh
./http-tunnel --config client.toml
```

Point `nas.example.com` to the public server's IP. Visiting `http://nas.example.com` will forward traffic to `127.0.0.1:80` on the client machine.

The client configuration path defaults to `client.toml` in the current directory. `remote`, `name`, and `token` can also be supplied through command-line options or the corresponding `HTTP_TUNNEL_REMOTE`, `HTTP_TUNNEL_NAME`, and `HTTP_TUNNEL_TOKEN` environment variables; command-line values take precedence over the file.

## Client configuration

A client can define multiple tunnels:

```toml
remote = "myserver.com:2333"
name = "home"
token = "replace-with-a-long-random-secret"
api_port = 8610          # Default: 8610; set to 0 to disable the API
heartbeat_timeout = 40   # Seconds; set to 0 to disable heartbeat timeout
retry_interval = 1       # Seconds between reconnect attempts
nodelay = true           # TCP_NODELAY; default: true

[[tunnels]]
name = "app1"             # Required; unique within this client
domain = "app1.example.com"
local_addr = "127.0.0.1:1081"

[[tunnels]]
name = "app2"
domain = "app2.example.com"
local_addr = "127.0.0.1:1082"
```

Each tunnel requires a unique client-local `name`, a `domain` to route, and a `local_addr` reachable from the client.

## Routing behavior

- Exact domains take precedence over wildcards. `*.example.com` matches subdomains at any depth, but not `example.com` itself.
- Host matching is case-insensitive; an optional port and a trailing dot are ignored. Absolute-form HTTP request targets are supported. Requests with multiple `Host` headers are rejected.
- Routes exist only while their client control channel is connected. An unknown route returns `404`; if no data channel becomes available within 10 seconds, the visitor receives `504`.
- Routing happens once per TCP connection, using its first request. A keep-alive connection cannot switch tunnels based on a later request's `Host`.
- Malformed requests, oversized headers, and slow headers are rejected.

## Client administration API

The client API listens on `8610` by default; use `--api-port` or `HTTP_TUNNEL_API_PORT` to change the port, or set it to `0` to disable the API. All endpoints require `Authorization: Bearer <token>`, using the shared tunnel token.

| Method | Path | Description |
| --- | --- | --- |
| `GET` | `/api/status` | Client connection status and tunnel count |
| `GET` | `/api/tunnels` | List tunnels |
| `PUT` | `/api/tunnels/{name}` | Create or replace a tunnel; JSON body: `{"domain":"nas.example.com","local_addr":"127.0.0.1:80"}` |
| `DELETE` | `/api/tunnels/{name}` | Delete a tunnel |

Example:

```sh
curl -X PUT http://127.0.0.1:8610/api/tunnels/nas \
  -H "Authorization: Bearer $CLIENT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"domain":"nas.example.com","local_addr":"127.0.0.1:80"}'
```

**Security:** the API binds to `0.0.0.0`. Do not expose it directly to the Internet. Restrict access with a firewall or private network, or place it behind a TLS-terminating reverse proxy. Use a strong token and protect the client configuration file.

## Docker

Run the public server:

```sh
docker run -d --name http-tunnel --restart unless-stopped \
  -p 2333:2333 -p 80:80 \
  -e HTTP_TUNNEL_TOKEN='replace-with-a-long-random-secret' \
  ghcr.io/jiangood/http-tunnel:latest server \
  --server-port 2333 --http-port 80
```

Run a client with `client.toml` in the current directory:

```sh
docker run -d --name http-tunnel-client --restart unless-stopped \
  -v "$PWD:/app" -w /app \
  ghcr.io/jiangood/http-tunnel:latest \
  --config /app/client.toml
```

If you enable the client API in Docker, publish its port only on a trusted interface and apply the security guidance above.

## Logging and tuning

Set `RUST_LOG` to `error`, `warn`, `info`, `debug`, or `trace`. The default is `info`.

```sh
RUST_LOG=error ./http-tunnel server --server-port 2333 --http-port 80 --token secret
```

TCP_NODELAY is enabled by default. Set `nodelay = false` to favor throughput over latency. Forwarding uses 64 KiB copy buffers on both server and client, so memory use increases with concurrent large transfers.

## Development

Run the throughput benchmark (it uses ports shared with integration tests, so don't run both simultaneously):

```sh
RUST_LOG=error cargo bench --bench throughput
```

See [Out of Scope](./docs/out-of-scope.md) for features this project intentionally does not implement.

## License

[Apache-2.0](./LICENSE)
