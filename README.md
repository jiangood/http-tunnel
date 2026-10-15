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

Download the [hts server](https://github.com/jiangood/http-tunnel/releases) and [htc client](https://github.com/jiangood/http-tunnel/releases), or use the Docker images below.

Both programs use a single fixed configuration file in the current working directory. They do not accept business configuration from command-line arguments or environment variables. The `-b` / `--background` option only controls how the process is launched.

### Server

Run the server once to generate `hts.toml`:

```sh
./hts
```

Edit `hts.toml` and set a strong shared token. The default ports are TCP `2333` for client connections and TCP `80` for public HTTP traffic. Then start the server again:

```sh
./hts
```

To run it in the background and append output to `hts.log`:

```sh
./hts -b
```

### Client

Run the client once to generate `htc.toml`:

```sh
./htc
```

Edit `htc.toml` with the server address, the same shared token, and your forwarding rules, then run `htc` again. To run it in the background and append output to `htc.log`, use `./htc -b`. For example:

```toml
remote = "myserver.com:2333"
name = "home"
token = "replace-with-the-same-secret-as-hts"
api_port = 8610
heartbeat_timeout = 40
retry_interval = 1
nodelay = true

[[tunnels]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
```

## Configuration

- `hts` reads only `hts.toml` from the current working directory.
- `htc` reads only `htc.toml` from the current working directory.
- If the file does not exist, the program creates an example configuration and exits. Edit the generated file, then restart the program.
- `--help` and `--version` remain available. `-b` / `--background` starts the process in the background; output is appended to `hts.log` or `htc.log` in the current directory. Business settings cannot be overridden through command-line flags or environment variables.
- Relative paths for any future file-based options are resolved relative to the configuration file.

Client configuration fields:

| Field | Meaning | Default |
| --- | --- | --- |
| `remote` | Server address, e.g. `myserver.com:2333` | Required |
| `name` | Client identity used in logs | Required |
| `token` | Shared secret, must match `hts.toml` | Required |
| `api_port` | Client administration API port; `0` disables it | `8610` |
| `heartbeat_timeout` | Heartbeat timeout in seconds; `0` disables it | `40` |
| `retry_interval` | Retry interval in seconds | `1` |
| `nodelay` | Enable TCP_NODELAY for tunnels | `true` |
| `tunnels` | Array of routes, each with `name`, `domain`, and `local_addr` | Empty |

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

The server and client are published as separate Linux amd64 images, each containing only its corresponding binary.

Start the server (ports default to TCP `2333` for clients and TCP `80` for HTTP traffic):

```sh
docker run -d --name http-tunnel-server --restart unless-stopped \
  -p 2333:2333 -p 80:80 \
  -v "$PWD/hts.toml:/app/hts.toml:ro" \
  ghcr.io/jiangood/hts:latest
```

Start the client by mounting its configuration at `/app/htc.toml`:

```sh
docker run -d --name http-tunnel-client --restart unless-stopped \
  -v "$PWD/htc.toml:/app/htc.toml:ro" \
  ghcr.io/jiangood/htc:latest
```

The client API writes configuration changes back to the file, so mount it read-write if you use the API to manage tunnels. The client connects to the server address and port configured in `htc.toml`.

## Operations

Logging defaults to `info`; logging levels are no longer configured through environment variables. TCP_NODELAY is enabled by default. Forwarding uses 64 KiB copy buffers on both ends.

Run the throughput benchmark with `cargo bench --bench throughput`. See [Out of Scope](./docs/out-of-scope.md) for intentional limitations.

## License

[Apache-2.0](./LICENSE)
