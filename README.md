# http-tunnel

[![GitHub stars](https://img.shields.io/github/stars/jiangood/http-tunnel)](https://github.com/jiangood/http-tunnel/stargazers)
[![GitHub release (latest SemVer)](https://img.shields.io/github/v/release/jiangood/http-tunnel)](https://github.com/jiangood/http-tunnel/releases)
![GitHub Workflow Status (branch)](https://img.shields.io/github/actions/workflow/status/jiangood/http-tunnel/rust.yml?branch=main)
[![GitHub all releases](https://img.shields.io/github/downloads/jiangood/http-tunnel/total)](https://github.com/jiangood/http-tunnel/releases)

A secure, stable and high-performance HTTP reverse proxy for NAT traversal, written in Rust

`http-tunnel` exposes HTTP services on a device behind the NAT to the Internet, via a server with a public IP. The server listens on a single HTTP port and routes each request to the right tunnel according to the `Host` header. The traffic between the server and the client is carried over a plain TCP tunnel.

The whole configuration lives on the server. A client is configured by the server: it takes no configuration file, and
the server pushes it the tunnels that it should serve.

> `http-tunnel` is a fork of [rathole](https://github.com/rathole-org/rathole), reworked into an HTTP reverse proxy.
> It is distributed under the Apache-2.0 license, see [LICENSE](./LICENSE).

<!-- TOC -->

- [http-tunnel](#http-tunnel)
  - [Features](#features)
  - [Quickstart](#quickstart)
  - [Docker](#docker)
  - [Configuration](#configuration)
    - [Routing](#routing)
    - [Logging](#logging)
    - [Tuning](#tuning)
  - [Administration API](#administration-api)
  - [Client Administration API](#client-administration-api)
  - [Planning](#planning)

<!-- /TOC -->

## Features

- **HTTP Reverse Proxy** One public HTTP port serves all the tunnels. Requests are routed to the correct tunnel by the `Host` header. WebSocket and other protocol upgrades work transparently, as the traffic is piped through as it is.
- **Single Point of Configuration** Only the server is configured. A client is started with the address of the server, its name and its token, and the server pushes it the tunnels to serve.
- **High Performance** Much higher throughput can be achieved than frp, and more stable when handling a large volume of connections.
- **Low Resource Consumption** Consumes much less memory than similar tools.
- **Security** Every client is authenticated with its own token, and a client can only serve the tunnels assigned to it on the server.

## Quickstart

A full-powered `http-tunnel` can be obtained from the [release](https://github.com/jiangood/http-tunnel/releases) page. Or [build from source](docs/build-guide.md) **for other platforms and minimizing the binary**. A [Docker image](https://github.com/jiangood/http-tunnel/pkgs/container/http-tunnel) is also available.

To use `http-tunnel`, you need a server with a public IP, and a device behind the NAT with some HTTP services to expose to the Internet.

Assuming you have a NAS at home behind the NAT, and want to expose its web UI at `nas.example.com`:

1. On the server which has a public IP

Create `server.toml` with the following content and accommodate it to your needs. If the path doesn't exist, the
server writes a minimal template with the three ports (and the client and tunnel sections commented out) and starts.

```toml
# server.toml
server_port = 2333 # `2333` specifies the port that http-tunnel listens for clients
http_port = 80 # `80` specifies the HTTP entrypoint that visitors connect to

[clients.home_nas] # The name of the client
token = "use_a_secret_that_only_you_know" # The token of the client

[clients.home_nas.tunnels]
"nas.example.com" = "127.0.0.1:80" # Requests with this `Host` are forwarded to the NAS web UI, as seen from the NAS
```

Then run it from the directory that contains `server.toml`:

```bash
./http-tunnel server
```

The configuration file is optional and defaults to `server.toml` in the current directory. A different path can also
be given explicitly:

```bash
./http-tunnel server /path/to/server.toml
```

2. On the host which is behind the NAT (your NAS)

The client needs no configuration file. Just tell it where the server is, which name it was given in the configuration
of the server, and its token:

```bash
./http-tunnel client --remote myserver.com:2333 --name home_nas --token use_a_secret_that_only_you_know
```

or pass the token in the environment, so that it doesn't show up in `ps`:

```bash
HTTP_TUNNEL_TOKEN=use_a_secret_that_only_you_know ./http-tunnel client --remote myserver.com:2333 --name home_nas
```

Every option can also be passed through an environment variable (`HTTP_TUNNEL_REMOTE`, `HTTP_TUNNEL_NAME`,
`HTTP_TUNNEL_TOKEN` and `HTTP_TUNNEL_API_PORT`), which suits a container:

```bash
HTTP_TUNNEL_REMOTE=myserver.com:2333 HTTP_TUNNEL_NAME=home_nas \
  HTTP_TUNNEL_TOKEN=use_a_secret_that_only_you_know ./http-tunnel client
```

The [client administration API](#client-administration-api), which maintains the client's tunnels at runtime, listens
on `8610` by default. Set `HTTP_TUNNEL_API_PORT` (or `--api-port`) to change it.

3. Now the client will try to connect to the server `myserver.com` on port `2333`, and the server pushes it the tunnels
   of `home_nas`, including the tunnel `nas.example.com`. Any HTTP request to the server on port `80` with `Host: nas.example.com` will be
   forwarded to the NAS on port `80`.

So you can visit `http://nas.example.com` (with `nas.example.com` resolving to your server) to reach the NAS web UI.

To run `http-tunnel` as a background service on Linux, checkout the [systemd examples](./examples/systemd). To run it in a container, see [Docker](#docker).

## Docker

A [Docker image](https://github.com/jiangood/http-tunnel/pkgs/container/http-tunnel) is published to the GitHub
Container Registry for `linux/amd64` and `linux/arm64`. It's a [distroless](https://github.com/GoogleContainerTools/distroless)
image, so it only contains `http-tunnel` and has no shell. The binary is installed at `/usr/local/bin/http-tunnel`,
outside the working directory, so mounting a directory over the working directory doesn't hide it. The container runs
as root, so it can write the bind-mounted `server.toml` whatever the owner of the host directory is.

The server reads `server.toml` from its working directory (`/app` in the image), which it also writes back to when
it's changed through the [administration API](#administration-api). Mount the directory that holds `server.toml`
read-write, and publish the ports of the config you use. No path has to be passed on the command line:

```bash
# server.toml
# server_port = 2333
# http_port = 80

docker run -d --name http-tunnel --restart unless-stopped \
  -p 2333:2333 -p 80:80 \
  -v "$PWD:/app" \
  ghcr.io/jiangood/http-tunnel:latest server
```

The mounted folder must be writable: if `server.toml` is missing, the server generates a minimal template there and
starts, and the administration API rewrites it in place. If the folder is read-only, pass an explicit path to a
writable location instead.

The server [administration API](#administration-api) is off unless `api_port` and `api_token` are set; publish that
port too (`-p 2335:2335`) when it's enabled.

The client takes no configuration file, so it only needs the server address, its name and its token. Pass them through
environment variables, which keeps the token out of the container's arguments:

```bash
docker run -d --name http-tunnel --restart unless-stopped \
  -p 8610:8610 \
  -e HTTP_TUNNEL_REMOTE=myserver.com:2333 \
  -e HTTP_TUNNEL_NAME=home_nas \
  -e HTTP_TUNNEL_TOKEN=use_a_secret_that_only_you_know \
  ghcr.io/jiangood/http-tunnel:latest client
```

The default bridge network is enough: the client only dials the server, so it doesn't need the host network. A tunnel
that reaches a service on the host uses `host.docker.internal` as its `local_addr` in `server.toml` (on Linux, add
`--add-host host.docker.internal:host-gateway` to the client container), or the name of another container on the same
network.

The client [administration API](#client-administration-api) listens on `8610` by default, so `-p 8610:8610` publishes
it. Drop the mapping if you don't need to manage the tunnels remotely. It's exposed on every interface, so protect it
with a reverse proxy that terminates TLS, a firewall rule, or a loopback-only mapping
(`-p 127.0.0.1:8610:8610`). Change the port with `HTTP_TUNNEL_API_PORT` (or `--api-port`).

## Configuration

All the configuration lives in one file, and it's the configuration of the server. Tunnels are grouped by the client
that serves them, and each client is identified by a name and authenticated by its own token.

Before heading to the full configuration specification, it's recommended to skim [the configuration examples](./examples) to get a feeling of the configuration format.

Here is the full configuration specification:

```toml
server_port = 2333 # Necessary. The port that the server listens for clients, on all interfaces
http_port = 80 # Necessary. The HTTP entrypoint. Visitors are routed by the `Host` header
api_port = 2335 # Optional. The administration API and the web UI, on all interfaces
api_token = "a_secret_for_the_admin_api" # Optional. The token required by the administration API. Required if `api_port` is set

[clients.home] # A client. The name `home` must be identical to the `--name` of the client
token = "use_a_secret_that_only_you_know" # Necessary. The token of the client. It can also be given by the `HTTP_TUNNEL_TOKEN` environment variable
heartbeat_interval = 30 # Optional. The interval between two application-layer heartbeats sent to this client. 0 disables them. Default: 30 seconds
heartbeat_timeout = 40 # Optional. The application-layer heartbeat timeout. Set to 0 to disable the test. The value must be greater than `heartbeat_interval`. Default: 40 seconds
retry_interval = 1 # Optional. The interval between retries of the client to connect to the server. Default: 1 second
nodelay = true # Optional. The default TCP_NODELAY of the tunnels of this client. Default: true

[clients.home.tunnels] # Each tunnel is a domain keyed to the `local_addr` of the local service
"app1.example.com" = "127.0.0.1:1081"
"app2.example.com" = "127.0.0.1:1082"
"www.app2.example.com" = "127.0.0.1:1082" # The same address can serve several domains

[clients.office] # Multiple clients can be defined. Each of them is started with `--name office`
token = "another_secret"
nodelay = false # Applied to all the tunnels of `office`

[clients.office.tunnels]
"app3.example.com" = "127.0.0.1:1083"
```

The client names are global, and a `Host` domain can only be claimed by one tunnel. A change of the config file still
needs a restart of the server and the clients, but the [administration API](#administration-api) applies the changes at
runtime instead.

### Routing

The server accepts HTTP connections on `http_port`. For every connection, it reads the HTTP request line and the `Host` header (without touching the body), and forwards the connection to the tunnel whose domain matches that host. Host matching is case-insensitive, the port, if any, is ignored, and the trailing dot of a fully qualified name (e.g. `nas.example.com.`) is stripped. An absolute-form request line (`GET http://host/path HTTP/1.1`, as sent by proxy clients) is routed by the authority of the target. A request with several `Host` headers is rejected with `400`.

A tunnel domain may be a wildcard, `*.example.com`, which matches any subdomain
at any depth but not the apex `example.com`. An exact domain always wins over a
wildcard, and among the wildcards the longest suffix wins.

If no tunnel matches the `Host`, the server responds with `404`. If the matched tunnel is not connected yet, it responds with `503`. If the tunnel is connected but the client does not provide a data channel within 10 seconds — for instance because it cannot reach its `local_addr` — the visitor is answered with `504` instead of being left hanging. A malformed request, an oversized header, and a header that is too slow to arrive are answered with `400`, `431`, and `408` respectively.

Routing is done once per connection, on its first request. A keep-alive connection cannot be re-routed to another tunnel if a later request carries a different `Host`. Because of that, the `Host` used for routing is the one of the first request of the connection; a proxy client that reuses a connection across origins will reach the backend of the first request.

### Logging

`http-tunnel`, like many other Rust programs, use environment variables to control the logging level. `info`, `warn`, `error`, `debug`, `trace` are available.

```shell
RUST_LOG=error ./http-tunnel server config.toml
```

will run `http-tunnel` with only error level logging.

If `RUST_LOG` is not present, the default logging level is `info`.

The logs name the reason a visitor could not be served, so that a failure can be
told apart from a routing miss: a malformed request is logged as a bad header, an
unknown `Host` as a routing miss, and a missing data channel as a timeout or a
closed control channel. The count of each outcome is also reported by the
[administration API](#administration-api) under `metrics`, together with the
bytes transferred per direction and the state of the data channel pool.

### Tuning

`http-tunnel` enables TCP_NODELAY by default, which should benefit the latency and interactive applications. However, it slightly decreases the bandwidth.

If the bandwidth is more important, TCP_NODELAY can be opted out with `nodelay = false` for a whole client.

The bytes between a visitor, the tunnel and the local service are piped through
64 KiB buffers on both the server and the client. Raising the tokio default
(8 KiB) reduces the number of syscalls on large transfers; the buffers are
allocated per forwarded connection, so the concurrency of large transfers is
what trades against memory.

## Administration API

The server can expose a small REST API and a web UI to manage the clients and their tunnels at runtime, without a
restart. It's disabled by default and is enabled by setting `api_port` and `api_token` in `server.toml`:

```toml
api_port = 2335
api_token = "a_secret_for_the_admin_api"
```

All the API routes require the header `Authorization: Bearer <api_token>`. Open `http://127.0.0.1:2335/` for a
minimal web UI. The API binds to `0.0.0.0`, which makes it reachable from the network, so protect it with a
reverse proxy that terminates TLS, a firewall rule, or a private network such as WireGuard; publish the port on the
host loopback only (e.g. `-p 127.0.0.1:2335:2335` in Docker) for local-only access.

| Method | Path | Description |
| --- | --- | --- |
| `GET` | `/api/status` | A summary of the running server, including the runtime metrics |
| `GET` | `/api/clients` | List the clients and their tunnels |
| `POST` | `/api/clients` | Create a client |
| `GET` | `/api/clients/{client}` | Get a client |
| `PATCH` | `/api/clients/{client}` | Update a client (token, heartbeat interval, heartbeat timeout, retry interval, nodelay) |
| `DELETE` | `/api/clients/{client}` | Delete a client and its tunnels |
| `GET` | `/api/clients/{client}/tunnels` | List the tunnels of a client |
| `PUT` | `/api/clients/{client}/tunnels/{domain}` | Create or replace a tunnel (`{local_addr}`) |
| `DELETE` | `/api/clients/{client}/tunnels/{domain}` | Delete a tunnel |

Every change is written back to the `server.toml` atomically, and is pushed to the connected clients, which start,
update or stop the corresponding tunnels without a restart. Because the file is rewritten, the comments of a
`server.toml` that is managed through the API are not preserved.

Two caveats: changing the `token` of a client only affects the server, so the client must be restarted with the new
`--token`; and the config channel of a client is authenticated by the token, so a client that reconnects with an old
token is rejected.

```bash
# Create a client, then add a tunnel to it
curl -X POST http://127.0.0.1:2335/api/clients \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"name":"home","token":"a_secret_token"}'

curl -X PUT http://127.0.0.1:2335/api/clients/home/tunnels/nas.example.com \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"local_addr":"127.0.0.1:80"}'
```

## Client Administration API

The client also exposes a REST API to maintain its own tunnels at runtime. It listens on `8610` by default; change the
port with `--api-port` or the `HTTP_TUNNEL_API_PORT` environment variable, or set it to `0` to disable the API:

```bash
./http-tunnel client --remote myserver.com:2333 --name home_nas \
  --token use_a_secret_that_only_you_know --api-port 2336
```

All the routes require `Authorization: Bearer <token>`, reusing the client token, so there is no second secret. The
server stays the source of truth: a change is forwarded over the config channel, validated and persisted by the
server, and pushed back to the client, which starts, updates or stops the tunnel without a restart of either side.

| Method | Path | Description |
| --- | --- | --- |
| `GET` | `/api/status` | The client identity, its connection state and its tunnel count |
| `GET` | `/api/tunnels` | List the tunnels the client is serving |
| `PUT` | `/api/tunnels/{domain}` | Create or replace a tunnel (`{local_addr}`) |
| `DELETE` | `/api/tunnels/{domain}` | Delete a tunnel |

```bash
curl -X PUT http://127.0.0.1:2336/api/tunnels/nas.example.com \
  -H "Authorization: Bearer $CLIENT_TOKEN" -H "Content-Type: application/json" \
  -d '{"local_addr":"127.0.0.1:80"}'
```

The API binds to `0.0.0.0`, so protect it the same way as the server API. See
[`docs/client-api.md`](./docs/client-api.md) for the details and the caveats.

## Planning

- [x] HTTP APIs for configuration
- [x] Client administration API to maintain a client's own tunnels

[Out of Scope](./docs/out-of-scope.md) lists features that are not planned to be implemented and why.

## Benchmarking

The HTTP path has a small harness in [`benches/`](./benches) that starts an echo
backend, a server and a client, and measures small requests, short-lived
connections, and concurrent 1 MiB bodies. It uses the same ports as the
integration tests, so it can't run at the same time as them:

```sh
RUST_LOG=error cargo bench --bench throughput
```
