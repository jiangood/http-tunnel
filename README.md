# http-tunnel

![http-tunnel-logo](./docs/img/http-tunnel-logo.png)

[![GitHub stars](https://img.shields.io/github/stars/jiangood/http-tunnel)](https://github.com/jiangood/http-tunnel/stargazers)
[![GitHub release (latest SemVer)](https://img.shields.io/github/v/release/jiangood/http-tunnel)](https://github.com/jiangood/http-tunnel/releases)
![GitHub Workflow Status (branch)](https://img.shields.io/github/actions/workflow/status/jiangood/http-tunnel/rust.yml?branch=main)
[![GitHub all releases](https://img.shields.io/github/downloads/jiangood/http-tunnel/total)](https://github.com/jiangood/http-tunnel/releases)

A secure, stable and high-performance HTTP reverse proxy for NAT traversal, written in Rust

`http-tunnel` exposes HTTP services on a device behind the NAT to the Internet, via a server with a public IP. The server listens on a single HTTP port and routes each request to the right service according to the `Host` header. The traffic between the server and the client is carried over a plain TCP tunnel.

The whole configuration lives on the server. A client is configured by the server: it takes no configuration file, and
the server pushes it the services that it should forward.

> `http-tunnel` is a fork of [rathole](https://github.com/rathole-org/rathole), reworked into an HTTP reverse proxy.
> It is distributed under the Apache-2.0 license, see [LICENSE](./LICENSE).

<!-- TOC -->

- [http-tunnel](#http-tunnel)
  - [Features](#features)
  - [Quickstart](#quickstart)
  - [Docker](#docker)
    - [Docker Compose](#docker-compose)
  - [Configuration](#configuration)
    - [Routing](#routing)
    - [Logging](#logging)
    - [Tuning](#tuning)
  - [Administration API](#administration-api)
  - [Benchmark](#benchmark)
  - [Planning](#planning)

<!-- /TOC -->

## Features

- **HTTP Reverse Proxy** One public HTTP port serves all services. Requests are routed to the correct service by the `Host` header. WebSocket and other protocol upgrades work transparently, as the traffic is piped through as it is.
- **Single Point of Configuration** Only the server is configured. A client is started with the address of the server, its name and its token, and the server pushes it the services to forward.
- **High Performance** Much higher throughput can be achieved than frp, and more stable when handling a large volume of connections.
- **Low Resource Consumption** Consumes much fewer memory than similar tools.
- **Security** Every client is authenticated with its own token, and a client can only serve the services that are assigned to it on the server.

## Quickstart

A full-powered `http-tunnel` can be obtained from the [release](https://github.com/jiangood/http-tunnel/releases) page. Or [build from source](docs/build-guide.md) **for other platforms and minimizing the binary**. A [Docker image](https://github.com/jiangood/http-tunnel/pkgs/container/http-tunnel) is also available.

To use `http-tunnel`, you need a server with a public IP, and a device behind the NAT, where some HTTP services that need to be exposed to the Internet.

Assuming you have a NAS at home behind the NAT, and want to expose its web UI at `nas.example.com`:

1. On the server which has a public IP

Create `server.toml` with the following content and accommodate it to your needs. If the path doesn't exist, the
server writes a minimal template with the three ports (and the client and service sections commented out) and starts.

```toml
# server.toml
bind_addr = "0.0.0.0:2333" # `2333` specifies the port that http-tunnel listens for clients
http_bind_addr = "0.0.0.0:80" # `80` specifies the HTTP entrypoint that visitors connect to

[clients.home_nas] # The name of the client
token = "use_a_secret_that_only_you_know" # The token of the client

[clients.home_nas.services.my_nas]
hosts = ["nas.example.com"] # Requests with this `Host` are forwarded to `my_nas`
local_addr = "127.0.0.1:80" # The address of the NAS web UI, as seen from the NAS
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

3. Now the client will try to connect to the server `myserver.com` on port `2333`, and the server pushes it the services
   of `home_nas`, including `my_nas`. Any HTTP request to the server on port `80` with `Host: nas.example.com` will be
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
# bind_addr = "0.0.0.0:2333"
# http_bind_addr = "0.0.0.0:80"

docker run -d --name http-tunnel --restart unless-stopped \
  -p 2333:2333 -p 80:80 \
  -v "$PWD:/app" \
  ghcr.io/jiangood/http-tunnel:latest server
```

The mounted folder must be writable: if `server.toml` is missing, the server generates a minimal template there and
starts, and the administration API rewrites it in place. If the folder is read-only, pass an explicit path to a
writable location instead.

The client takes no configuration file, so it only needs the arguments:

```bash
docker run -d --name http-tunnel --restart unless-stopped --network host \
  ghcr.io/jiangood/http-tunnel:latest client \
  --remote myserver.com:2333 --name home_nas --token use_a_secret_that_only_you_know
```

`--network host` is the simplest way for the client to reach the services on the host. On Docker Desktop a client
that has to reach a service on the host uses `host.docker.internal` as the `local_addr` instead.

If the [administration API](#administration-api) is enabled, publish its port and set `api_bind_addr` to
`0.0.0.0:2335`, otherwise the container only listens on its own loopback and Docker cannot forward the connections:

```bash
  -p 2335:2335
```

This exposes the admin API on every interface of the host. Put it behind a reverse proxy with TLS, restrict the source
addresses with a firewall, or bind it to the loopback address (`-p 127.0.0.1:2335:2335` together with
`api_bind_addr = "127.0.0.1:2335"`) when it's only managed locally.

The image only contains the binary: it's assembled from the static musl build, so nothing is compiled inside it. To
build it locally, compile the musl binary and place it under `build-out/<arch>/` first:

```bash
cargo build --release --target x86_64-unknown-linux-musl
mkdir -p build-out/amd64
cp target/x86_64-unknown-linux-musl/release/http-tunnel build-out/amd64/
docker build --build-arg TARGETARCH=amd64 -t http-tunnel .
```

The token can also be passed through the `HTTP_TUNNEL_TOKEN` environment variable, which keeps it out of the
container's arguments:

```bash
docker run -d --name http-tunnel --network host \
  -e HTTP_TUNNEL_TOKEN=use_a_secret_that_only_you_know \
  ghcr.io/jiangood/http-tunnel:latest client --remote myserver.com:2333 --name home_nas
```

### Docker Compose

The same containers can be managed with Docker Compose. On the server, put this `docker-compose.yml` in the directory
that holds `server.toml` (or use [`examples/docker-compose/server`](./examples/docker-compose/server)):

```yaml
# docker-compose.yml
services:
  http-tunnel:
    image: ghcr.io/jiangood/http-tunnel:latest
    container_name: http-tunnel
    restart: unless-stopped
    command: server
    ports:
      - "2333:2333" # Clients connect here
      - "80:80" # Visitors connect here
      - "2335:2335" # Administration API, exposed on all interfaces, drop unless it's enabled
    volumes:
      - ./:/app # Holds server.toml, must be writable
```

Then start it with:

```bash
docker compose up -d
```

On the host behind the NAT (the client), use a second `docker-compose.yml`, or
[`examples/docker-compose/client`](./examples/docker-compose/client). It takes no configuration file, so the server
address, name and token are passed as arguments and environment variables:

```yaml
# docker-compose.yml
services:
  http-tunnel:
    image: ghcr.io/jiangood/http-tunnel:latest
    container_name: http-tunnel
    restart: unless-stopped
    network_mode: host
    command: client --remote myserver.com:2333 --name home_nas
    environment:
      - HTTP_TUNNEL_TOKEN=use_a_secret_that_only_you_know
```

To build the image locally instead of pulling it, keep the `build` section and the `build-out/<arch>/` layout
described above:

```yaml
services:
  http-tunnel:
    build:
      context: .
      args:
        TARGETARCH: amd64
    # ...
```

Ready-to-use files are in [`examples/docker-compose`](./examples/docker-compose), with a directory for the
[server](./examples/docker-compose/server) and one for the [client](./examples/docker-compose/client).

## Configuration

All the configuration lives in one file, and it's the configuration of the server. Services are grouped by the client
that serves them, and each client is identified by a name and authenticated by its own token.

Before heading to the full configuration specification, it's recommend to skim [the configuration examples](./examples) to get a feeling of the configuration format.

Here is the full configuration specification:

```toml
bind_addr = "0.0.0.0:2333" # Necessary. The address that the server listens for clients. Generally only the port needs to be change.
http_bind_addr = "0.0.0.0:80" # Necessary. The HTTP entrypoint. Visitors are routed by the `Host` header
heartbeat_interval = 30 # Optional. The interval between two application-layer heartbeat. Set to 0 to disable sending heartbeat. Default: 30 seconds
api_bind_addr = "0.0.0.0:2335" # Optional. The address of the administration API and the web UI. Disabled if not set and exposed to the network when bound to 0.0.0.0
api_token = "a_secret_for_the_admin_api" # Optional. The token required by the administration API. Required if `api_bind_addr` is set

[clients.home] # A client. The name `home` must be identical to the `--name` of the client
token = "use_a_secret_that_only_you_know" # Necessary. The token of the client. It can also be given by the `HTTP_TUNNEL_TOKEN` environment variable
heartbeat_timeout = 40 # Optional. Set to 0 to disable the application-layer heartbeat test. The value must be greater than `heartbeat_interval`. Default: 40 seconds
retry_interval = 1 # Optional. The interval between retries of the client to connect to the server. Default: 1 second
nodelay = true # Optional. The default TCP_NODELAY of the services of this client. Default: true

[clients.home.services.service1] # A service of `home`. The name `service1` can change arbitrarily
hosts = ["service1.example.com"] # Necessary. Requests with these `Host` values are routed to this service
local_addr = "127.0.0.1:1081" # Necessary. The address of the local HTTP service on the client side
nodelay = true # Optional. Determine whether to enable TCP_NODELAY, to improve the latency but decrease the bandwidth. Default: inherits the client
retry_interval = 1 # Optional. The interval between retries to connect to the server. Default: inherits the client

[clients.home.services.service2] # Multiple services can be defined
hosts = ["service2.example.com", "www.service2.example.com"]
local_addr = "127.0.0.1:1082"

[clients.office] # Multiple clients can be defined. Each of them is started with `--name office`
token = "another_secret"
nodelay = false # Applied to all the services of `office` unless a service overrides it

[clients.office.services.service3]
hosts = ["service3.example.com"]
local_addr = "127.0.0.1:1083"
```

The names of the services are global: two clients cannot define a service with the same name, and the same `Host`
cannot be claimed by two services. A change of the config file still needs a restart of the server and the clients,
but the [administration API](#administration-api) applies the changes at runtime instead.

### Routing

The server accepts HTTP connections on `http_bind_addr`. For every connection, it reads the HTTP request line and the `Host` header (without touching the body), and forwards the connection to the service whose `hosts` contains that host. Host matching is case-insensitive and the port, if any, is ignored.

If no service matches the `Host`, the server responds with `404`. If the matched service is not connected yet, it responds with `503`.

Routing is done once per connection, on its first request. A keep-alive connection cannot be re-routed to another service if a later request carries a different `Host`.

### Logging

`http-tunnel`, like many other Rust programs, use environment variables to control the logging level. `info`, `warn`, `error`, `debug`, `trace` are available.

```shell
RUST_LOG=error ./http-tunnel server config.toml
```

will run `http-tunnel` with only error level logging.

If `RUST_LOG` is not present, the default logging level is `info`.

### Tuning

`http-tunnel` enables TCP_NODELAY by default, which should benefit the latency and interactive applications. However, it slightly decreases the bandwidth.

If the bandwidth is more important, TCP_NODELAY can be opted out with `nodelay = false`, either for a whole client or
per service.

## Administration API

The server can expose a small REST API and a web UI to manage the clients and their services at runtime, without a
restart. It's disabled by default and is enabled by setting `api_bind_addr` and `api_token` in `server.toml`:

```toml
api_bind_addr = "0.0.0.0:2335"
api_token = "a_secret_for_the_admin_api"
```

All the API routes require the header `Authorization: Bearer <api_token>`. Open `http://127.0.0.1:2335/` for a
minimal web UI. Binding to `0.0.0.0` makes the API reachable from the network, so protect it with a reverse proxy that
terminates TLS, a firewall rule, or a private network such as WireGuard; use `127.0.0.1:2335` for local-only access.

| Method | Path | Description |
| --- | --- | --- |
| `GET` | `/api/status` | A summary of the running server |
| `GET` | `/api/clients` | List the clients and their services |
| `POST` | `/api/clients` | Create a client |
| `GET` | `/api/clients/{client}` | Get a client |
| `PATCH` | `/api/clients/{client}` | Update a client (token, heartbeat timeout, retry interval, nodelay) |
| `DELETE` | `/api/clients/{client}` | Delete a client and its services |
| `GET` | `/api/clients/{client}/services` | List the services of a client |
| `PUT` | `/api/clients/{client}/services/{service}` | Create or replace a service |
| `DELETE` | `/api/clients/{client}/services/{service}` | Delete a service |

Every change is written back to the `server.toml` atomically, and is pushed to the connected clients, which start,
update or stop the corresponding tunnels without a restart. Because the file is rewritten, the comments of a
`server.toml` that is managed through the API are not preserved.

Two caveats: changing the `token` of a client only affects the server, so the client must be restarted with the new
`--token`; and the config channel of a client is authenticated by the token, so a client that reconnects with an old
token is rejected.

```bash
# Create a client, then add a service to it
curl -X POST http://127.0.0.1:2335/api/clients \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"name":"home","token":"a_secret_token"}'

curl -X PUT http://127.0.0.1:2335/api/clients/home/services/nas \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"hosts":["nas.example.com"],"local_addr":"127.0.0.1:80"}'
```

## Benchmark

`http-tunnel` has similar latency to [frp](https://github.com/fatedier/frp), but can handle a more connections, provide larger bandwidth, with less memory usage.

For more details, see the separate page [Benchmark](./docs/benchmark.md).

![http_throughput](./docs/img/http_throughput.svg)
![tcp_bitrate](./docs/img/tcp_bitrate.svg)
![mem](./docs/img/mem-graph.png)

## Planning

- [x] HTTP APIs for configuration

[Out of Scope](./docs/out-of-scope.md) lists features that are not planned to be implemented and why.
