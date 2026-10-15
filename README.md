# http-tunnel

[![GitHub stars](https://img.shields.io/github/stars/jiangood/http-tunnel)](https://github.com/jiangood/http-tunnel/stargazers)
[![GitHub release (latest SemVer)](https://img.shields.io/github/v/release/jiangood/http-tunnel)](https://github.com/jiangood/http-tunnel/releases)
![GitHub Workflow Status (branch)](https://img.shields.io/github/actions/workflow/status/jiangood/http-tunnel/rust.yml?branch=main)
[![GitHub all releases](https://img.shields.io/github/downloads/jiangood/http-tunnel/total)](https://github.com/jiangood/http-tunnel/releases)

A secure, stable and high-performance HTTP reverse proxy for NAT traversal, written in Rust

`http-tunnel` exposes HTTP services on a device behind the NAT to the Internet, via a server with a public IP. The server listens on a single HTTP port and routes each request to the right tunnel according to the `Host` header. The traffic between the server and the client is carried over a plain TCP tunnel.

The whole configuration lives on the clients. The server takes no configuration file: it's started with its
two ports and a shared token, and it learns the tunnels from the clients as they connect. A client declares
the tunnels it serves in its own configuration file, so every client manages its own domains.

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
  - [Client Administration API](#client-administration-api)
  - [Planning](#planning)
  - [Benchmarking](#benchmarking)

<!-- /TOC -->

## Features

- **HTTP Reverse Proxy** One public HTTP port serves all the tunnels. Requests are routed to the correct tunnel by the `Host` header. WebSocket and other protocol upgrades work transparently, as the traffic is piped through as it is.
- **Configuration on the Client** The server takes no configuration file. It's started with its ports and a shared token, and it learns the tunnels from the clients. Each client declares its own tunnels in a `client.toml`.
- **High Performance** Much higher throughput can be achieved than frp, and more stable when handling a large volume of connections.
- **Low Resource Consumption** Consumes much less memory than similar tools.
- **Security** Every client is authenticated with the shared token. A domain is served by a single tunnel at a time.

## Quickstart

A full-powered `http-tunnel` can be obtained from the [release](https://github.com/jiangood/http-tunnel/releases) page. Or [build from source](docs/build-guide.md) **for other platforms and minimizing the binary**. A [Docker image](https://github.com/jiangood/http-tunnel/pkgs/container/http-tunnel) is also available.

To use `http-tunnel`, you need a server with a public IP, and a device behind the NAT with some HTTP services to expose to the Internet.

Assuming you have a NAS at home behind the NAT, and want to expose its web UI at `nas.example.com`:

1. On the server which has a public IP

Start the server with the port it listens on for clients, the HTTP entrypoint, and a shared token. The server
reads no configuration file:

```bash
./http-tunnel server --server-port 2333 --http-port 80 \
  --token use_a_secret_that_only_you_know
```

Every option can also be passed through an environment variable (`HTTP_TUNNEL_SERVER_PORT`,
`HTTP_TUNNEL_HTTP_PORT` and `HTTP_TUNNEL_TOKEN`), which keeps the token out of `ps` and suits a container:

```bash
HTTP_TUNNEL_TOKEN=use_a_secret_that_only_you_know ./http-tunnel server --server-port 2333 --http-port 80
```

2. On the host which is behind the NAT (your NAS)

Create `client.toml` with the following content and accommodate it to your needs:

```toml
# client.toml
remote = "myserver.com:2333" # The address of the server
name = "home_nas" # The name of the client, used for logging
token = "use_a_secret_that_only_you_know" # The shared token of the server

[tunnels]
"nas.example.com" = "127.0.0.1:80" # Requests with this `Host` are forwarded to the NAS web UI, as seen from the NAS
```

Then run it from the directory that contains `client.toml`. The client is the default role, so no subcommand is
needed (the server is run with `./http-tunnel server`):

```bash
./http-tunnel
```

The configuration file is optional and defaults to `client.toml` in the current directory. A different path can
also be given explicitly:

```bash
./http-tunnel --config /path/to/client.toml
```

The identity (`remote`, `name` and `token`) can also be given on the command line or through the environment
(`HTTP_TUNNEL_REMOTE`, `HTTP_TUNNEL_NAME`, `HTTP_TUNNEL_TOKEN` and `HTTP_TUNNEL_API_PORT`), in which case the
command line wins over the file. The tunnels, however, live only in the file:

```bash
HTTP_TUNNEL_TOKEN=use_a_secret_that_only_you_know ./http-tunnel \
  --remote myserver.com:2333 --name home_nas
```

The [client administration API](#client-administration-api), which maintains the client's own tunnels at
runtime, listens on `8610` by default. Set `HTTP_TUNNEL_API_PORT` (or `--api-port`) to change it.

3. Now the client will try to connect to the server `myserver.com` on port `2333`, and register its tunnel
   `nas.example.com`. Any HTTP request to the server on port `80` with `Host: nas.example.com` will be
   forwarded to the NAS on port `80`.

So you can visit `http://nas.example.com` (with `nas.example.com` resolving to your server) to reach the NAS web UI.

To run `http-tunnel` in a container, see [Docker](#docker).

## Docker

A [Docker image](https://github.com/jiangood/http-tunnel/pkgs/container/http-tunnel) is published to the GitHub
Container Registry for `linux/amd64` and `linux/arm64`. It's a [distroless](https://github.com/GoogleContainerTools/distroless)
image, so it only contains `http-tunnel` and has no shell. The binary is installed at `/usr/local/bin/http-tunnel`.

The server takes no configuration file, so it only needs its ports and the token. Pass the token through the
environment, which keeps it out of the container's arguments:

```bash
docker run -d --name http-tunnel --restart unless-stopped \
  -p 2333:2333 -p 80:80 \
  -e HTTP_TUNNEL_TOKEN=use_a_secret_that_only_you_know \
  ghcr.io/jiangood/http-tunnel:latest server --server-port 2333 --http-port 80
```

The client reads its tunnels from `client.toml`, so mount the directory that holds it read-write and publish
its administration API port. No path has to be passed on the command line:

```bash
# client.toml
# remote = "myserver.com:2333"
# name = "home_nas"
# token = "use_a_secret_that_only_you_know"
#
# [tunnels]
# "nas.example.com" = "127.0.0.1:80"

docker run -d --name http-tunnel --restart unless-stopped \
  -p 8610:8610 \
  -v "$PWD:/app" \
  ghcr.io/jiangood/http-tunnel:latest
```

The default bridge network is enough: the client only dials the server, so it doesn't need the host network. A tunnel
that reaches a service on the host uses `host.docker.internal` as its `local_addr` in `client.toml` (on Linux, add
`--add-host host.docker.internal:host-gateway` to the client container), or the name of another container on the same
network.

The mounted folder must be writable: a change made through the client [administration
API](#client-administration-api) rewrites `client.toml` in place. If the folder is read-only, pass an explicit path
to a writable location instead, or start the client with `--api-port 0`.

The client [administration API](#client-administration-api) listens on `8610` by default, so `-p 8610:8610` publishes
it. Drop the mapping if you don't need to manage the tunnels remotely. It's exposed on every interface, so protect it
with a reverse proxy that terminates TLS, a firewall rule, or a loopback-only mapping
(`-p 127.0.0.1:8610:8610`). Change the port with `HTTP_TUNNEL_API_PORT` (or `--api-port`).

## Configuration

All the configuration lives on the clients. The server takes no configuration file: it's started with its ports
and a shared token, and it learns the tunnels of a client from the control channels the client opens. A client
declares its own tunnels in its configuration file.

Before heading to the full configuration specification, it's recommended to skim [the configuration examples](./examples) to get a feeling of the configuration format.

Here is the full configuration specification:

The `tunnels` section is an array of tables: each tunnel carries its own `name`,
`domain` and `local_addr`. The `name` is the identity of the tunnel within the
client and is what the administration API addresses; the `domain` is the `Host`
the tunnel serves, unique among the client's tunnels. The `name` is optional and
defaults to the `domain`.

```toml
remote = "myserver.com:2333" # Necessary. The address of the server
name = "home" # Necessary. The name of the client, used for logging
token = "use_a_secret_that_only_you_know" # Necessary. The shared token of the server
api_port = 8610 # Optional. The port of the client administration API. 0 disables it. Default: 8610
heartbeat_timeout = 40 # Optional. The application-layer heartbeat timeout. Set to 0 to disable the test. Default: 40 seconds
retry_interval = 1 # Optional. The interval between retries to connect to the server. Default: 1 second
nodelay = true # Optional. Whether to enable TCP_NODELAY for the tunnels. Default: true

[[tunnels]] # Each tunnel is a table with its own name, domain and local_addr
name = "app1" # Optional. The identity of the tunnel within the client. Default: the domain
domain = "app1.example.com" # The `Host` the tunnel serves
local_addr = "127.0.0.1:1081" # The address of the local service

[[tunnels]]
name = "app2"
domain = "app2.example.com"
local_addr = "127.0.0.1:1082"

[[tunnels]]
domain = "www.app2.example.com" # The same address can serve several domains
local_addr = "127.0.0.1:1082"
```

The client names are global. A `Host` domain can only be claimed by one tunnel: a domain is registered on the
server while the control channel of its tunnel is connected, and a second client that connects a control channel
for the same domain takes it over (they share one token, so they are trusted). A change of the configuration
file still needs a restart of the client, but a change made through the
[client administration API](#client-administration-api) applies at runtime instead.

### Routing

The server accepts HTTP connections on `http_port`. For every connection, it reads the HTTP request line and the `Host` header (without touching the body), and forwards the connection to the tunnel whose domain matches that host. Host matching is case-insensitive, the port, if any, is ignored, and the trailing dot of a fully qualified name (e.g. `nas.example.com.`) is stripped. An absolute-form request line (`GET http://host/path HTTP/1.1`, as sent by proxy clients) is routed by the authority of the target. A request with several `Host` headers is rejected with `400`.

A tunnel domain may be a wildcard, `*.example.com`, which matches any subdomain
at any depth but not the apex `example.com`. An exact domain always wins over a
wildcard, and among the wildcards the longest suffix wins.

The server has no configuration file, so a domain is known only while the control channel of its tunnel is
connected. If no tunnel matches the `Host`, the server responds with `404`; a domain whose client is
disconnected is simply no longer claimed, so it is answered with `404` as well. If the tunnel is connected
but no data channel is available within 10 seconds, the visitor is answered with `504` instead of being left
hanging. A `504` is also returned when the client takes the data channel but closes it without sending a
response — which is how a client that cannot reach its `local_addr` fails — instead of the visitor getting an
empty reply. A malformed request, an oversized header, and a header that is too slow to arrive are answered
with `400`, `431`, and `408` respectively.

Routing is done once per connection, on its first request. A keep-alive connection cannot be re-routed to another tunnel if a later request carries a different `Host`. Because of that, the `Host` used for routing is the one of the first request of the connection; a proxy client that reuses a connection across origins will reach the backend of the first request.

### Logging

`http-tunnel`, like many other Rust programs, use environment variables to control the logging level. `info`, `warn`, `error`, `debug`, `trace` are available.

```shell
RUST_LOG=error ./http-tunnel server --server-port 2333 --http-port 80 --token secret
```

will run `http-tunnel` with only error level logging.

If `RUST_LOG` is not present, the default logging level is `info`.

The logs name the reason a visitor could not be served, so that a failure can be told apart from a routing miss:
a malformed request is logged as a bad header, an unknown `Host` as a routing miss, and a missing data channel as
a timeout or a tunnel that closes the data channel without responding. A failure is logged with the tunnel.

### Tuning

`http-tunnel` enables TCP_NODELAY by default, which should benefit the latency and interactive applications. However, it slightly decreases the bandwidth.

If the bandwidth is more important, TCP_NODELAY can be opted out with `nodelay = false` for a whole client.

The bytes between a visitor, the tunnel and the local service are piped through
64 KiB buffers on both the server and the client. Raising the tokio default
(8 KiB) reduces the number of syscalls on large transfers; the buffers are
allocated per forwarded connection, so the concurrency of large transfers is
what trades against memory.

## Client Administration API

The client exposes a REST API to maintain its own tunnels at runtime. It listens on `8610` by default; change the
port with `--api-port` or the `HTTP_TUNNEL_API_PORT` environment variable, or set it to `0` to disable the API:

```bash
./http-tunnel --config client.toml --api-port 2336
```

All the routes require `Authorization: Bearer <token>`, reusing the shared token, so there is no second secret. The
client is the source of truth: a change is validated and written back to the client configuration file, and applied
to the running tunnels without a restart.

| Method | Path | Description |
| --- | --- | --- |
| `GET` | `/api/status` | The client identity, its connection state and its tunnel count |
| `GET` | `/api/tunnels` | List the tunnels the client is serving |
| `PUT` | `/api/tunnels/{name}` | Create or replace a tunnel (`{domain, local_addr}`) |
| `DELETE` | `/api/tunnels/{name}` | Delete a tunnel |

`{name}` is the identity of the tunnel (`{name}`, not its domain). A tunnel that
was declared without a name is addressed by its domain.

```bash
curl -X PUT http://127.0.0.1:2336/api/tunnels/nas \
  -H "Authorization: Bearer $CLIENT_TOKEN" -H "Content-Type: application/json" \
  -d '{"domain":"nas.example.com","local_addr":"127.0.0.1:80"}'
```

The API binds to `0.0.0.0`, so protect it with a reverse proxy that terminates TLS,
a firewall rule, or a private network such as WireGuard. See
[`docs/client-api.md`](./docs/client-api.md) for the details and the caveats.

## Planning

- [x] Client administration API to maintain a client's own tunnels
- [x] Configuration on the client, with a configuration-free server

[Out of Scope](./docs/out-of-scope.md) lists features that are not planned to be implemented and why.

## Benchmarking

The HTTP path has a small harness in [`benches/`](./benches) that starts an echo
backend, a server and a client, and measures small requests, short-lived
connections, and concurrent 1 MiB bodies. It uses the same ports as the
integration tests, so it can't run at the same time as them:

```sh
RUST_LOG=error cargo bench --bench throughput
```
