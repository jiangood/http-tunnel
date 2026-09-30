# rathole

![rathole-logo](./docs/img/rathole-logo.png)

[![GitHub stars](https://img.shields.io/github/stars/rapiz1/rathole)](https://github.com/rapiz1/rathole/stargazers)
[![GitHub release (latest SemVer)](https://img.shields.io/github/v/release/rapiz1/rathole)](https://github.com/rapiz1/rathole/releases)
![GitHub Workflow Status (branch)](https://img.shields.io/github/actions/workflow/status/rapiz1/rathole/rust.yml?branch=main)
[![GitHub all releases](https://img.shields.io/github/downloads/rapiz1/rathole/total)](https://github.com/rapiz1/rathole/releases)
![Docker Pulls](https://img.shields.io/docker/pulls/rapiz1/rathole)

[English](README.md) | [简体中文](README-zh.md)

A secure, stable and high-performance HTTP reverse proxy for NAT traversal, written in Rust

`rathole` exposes HTTP services on a device behind the NAT to the Internet, via a server with a public IP. The server listens on a single HTTP port and routes each request to the right service according to the `Host` header. The traffic between the server and the client is carried over a plain TCP tunnel.

<!-- TOC -->

- [rathole](#rathole)
  - [Features](#features)
  - [Quickstart](#quickstart)
  - [Configuration](#configuration)
    - [Routing](#routing)
    - [Logging](#logging)
    - [Tuning](#tuning)
  - [Benchmark](#benchmark)
  - [Planning](#planning)

<!-- /TOC -->

## Features

- **HTTP Reverse Proxy** One public HTTP port serves all services. Requests are routed to the correct service by the `Host` header. WebSocket and other protocol upgrades work transparently, as the traffic is piped through as it is.
- **High Performance** Much higher throughput can be achieved than frp, and more stable when handling a large volume of connections.
- **Low Resource Consumption** Consumes much fewer memory than similar tools.
- **Security** Tokens of services are mandatory and service-wise. The server and clients are responsible for their own configs.

## Quickstart

A full-powered `rathole` can be obtained from the [release](https://github.com/rapiz1/rathole/releases) page. Or [build from source](docs/build-guide.md) **for other platforms and minimizing the binary**. A [Docker image](https://hub.docker.com/r/rapiz1/rathole) is also available.

To use `rathole`, you need a server with a public IP, and a device behind the NAT, where some HTTP services that need to be exposed to the Internet.

Assuming you have a NAS at home behind the NAT, and want to expose its web UI at `nas.example.com`:

1. On the server which has a public IP

Create `server.toml` with the following content and accommodate it to your needs.

```toml
# server.toml
[server]
bind_addr = "0.0.0.0:2333" # `2333` specifies the port that rathole listens for clients
http_bind_addr = "0.0.0.0:80" # `80` specifies the HTTP entrypoint that visitors connect to
default_token = "use_a_secret_that_only_you_know"

[server.services.my_nas]
hosts = ["nas.example.com"] # Requests with this `Host` are forwarded to `my_nas`
```

Then run:

```bash
./rathole server.toml
```

2. On the host which is behind the NAT (your NAS)

Create `client.toml` with the following content and accommodate it to your needs.

```toml
# client.toml
[client]
remote_addr = "myserver.com:2333" # The address of the server. The port must be the same with the port in `server.bind_addr`
default_token = "use_a_secret_that_only_you_know"

[client.services.my_nas]
local_addr = "127.0.0.1:80" # The address of the local HTTP service that needs to be forwarded
```

Then run:

```bash
./rathole client.toml
```

3. Now the client will try to connect to the server `myserver.com` on port `2333`. Any HTTP request to the server on port `80` with `Host: nas.example.com` will be forwarded to the client's port `80`.

So you can visit `http://nas.example.com` (with `nas.example.com` resolving to your server) to reach the NAS web UI.

To run `rathole` as a background service on Linux, checkout the [systemd examples](./examples/systemd).

## Configuration

`rathole` can automatically determine to run in the server mode or the client mode, according to the content of the configuration file, if only one of `[server]` and `[client]` block is present, like the example in [Quickstart](#quickstart).

But the `[client]` and `[server]` block can also be put in one file. Then on the server side, run `rathole --server config.toml` and on the client side, run `rathole --client config.toml` to explicitly tell `rathole` the running mode.

Before heading to the full configuration specification, it's recommend to skim [the configuration examples](./examples) to get a feeling of the configuration format.

Here is the full configuration specification:

```toml
[client]
remote_addr = "example.com:2333" # Necessary. The address of the server
default_token = "default_token_if_not_specify" # Optional. The default token of services, if they don't define their own ones
heartbeat_timeout = 40 # Optional. Set to 0 to disable the application-layer heartbeat test. The value must be greater than `server.heartbeat_interval`. Default: 40 seconds
retry_interval = 1 # Optional. The interval between retry to connect to the server. Default: 1 second

[client.services.service1] # A service that needs forwarding. The name `service1` can change arbitrarily, as long as identical to the name in the server's configuration
token = "whatever" # Necessary if `client.default_token` not set
local_addr = "127.0.0.1:1081" # Necessary. The address of the local HTTP service that needs to be forwarded
nodelay = true # Optional. Determine whether to enable TCP_NODELAY, to improve the latency but decrease the bandwidth. Default: true
retry_interval = 1 # Optional. The interval between retry to connect to the server. Default: inherits the global config

[client.services.service2] # Multiple services can be defined
local_addr = "127.0.0.1:1082"

[server]
bind_addr = "0.0.0.0:2333" # Necessary. The address that the server listens for clients. Generally only the port needs to be change.
http_bind_addr = "0.0.0.0:80" # Necessary. The HTTP entrypoint. Visitors are routed by the `Host` header
default_token = "default_token_if_not_specify" # Optional
heartbeat_interval = 30 # Optional. The interval between two application-layer heartbeat. Set to 0 to disable sending heartbeat. Default: 30 seconds

[server.services.service1] # The service name must be identical to the client side
token = "whatever" # Necessary if `server.default_token` not set
hosts = ["service1.example.com"] # Necessary. Requests with these `Host` values are routed to this service
nodelay = true # Optional. Same as the client

[server.services.service2]
hosts = ["service2.example.com", "www.service2.example.com"]
```

### Routing

The server accepts HTTP connections on `http_bind_addr`. For every connection, it reads the HTTP request line and the `Host` header (without touching the body), and forwards the connection to the service whose `hosts` contains that host. Host matching is case-insensitive and the port, if any, is ignored.

If no service matches the `Host`, the server responds with `404`. If the matched service is not connected yet, it responds with `503`.

Routing is done once per connection, on its first request. A keep-alive connection cannot be re-routed to another service if a later request carries a different `Host`.

### Logging

`rathole`, like many other Rust programs, use environment variables to control the logging level. `info`, `warn`, `error`, `debug`, `trace` are available.

```shell
RUST_LOG=error ./rathole config.toml
```

will run `rathole` with only error level logging.

If `RUST_LOG` is not present, the default logging level is `info`.

### Tuning

`rathole` enables TCP_NODELAY by default, which should benefit the latency and interactive applications. However, it slightly decreases the bandwidth.

If the bandwidth is more important, TCP_NODELAY can be opted out with `nodelay = false`, either globally per service.

## Benchmark

`rathole` has similar latency to [frp](https://github.com/fatedier/frp), but can handle a more connections, provide larger bandwidth, with less memory usage.

For more details, see the separate page [Benchmark](./docs/benchmark.md).

![http_throughput](./docs/img/http_throughput.svg)
![tcp_bitrate](./docs/img/tcp_bitrate.svg)
![mem](./docs/img/mem-graph.png)

## Planning

- [ ] HTTP APIs for configuration

[Out of Scope](./docs/out-of-scope.md) lists features that are not planned to be implemented and why.
