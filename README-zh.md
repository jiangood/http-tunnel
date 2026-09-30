# rathole

![rathole-logo](./docs/img/rathole-logo.png)

[![GitHub stars](https://img.shields.io/github/stars/rapiz1/rathole)](https://github.com/rapiz1/rathole/stargazers)
[![GitHub release (latest SemVer)](https://img.shields.io/github/v/release/rapiz1/rathole)](https://github.com/rapiz1/rathole/releases)
![GitHub Workflow Status (branch)](https://img.shields.io/github/actions/workflow/status/rapiz1/rathole/rust.yml?branch=main)
[![GitHub all releases](https://img.shields.io/github/downloads/rapiz1/rathole/total)](https://github.com/rapiz1/rathole/releases)
[![Docker Pulls](https://img.shields.io/docker/pulls/rapiz1/rathole)](https://hub.docker.com/r/rapiz1/rathole)

[English](README.md) | [简体中文](README-zh.md)

安全、稳定、高性能的 HTTP 内网穿透工具，用 Rust 语言编写

`rathole` 可以将 NAT 后的设备上的 HTTP 服务通过具有公网 IP 的服务器暴露在公网上。服务端只监听一个 HTTP 端口，并根据请求的 `Host` 头把请求路由到对应的服务。服务端与客户端之间的流量通过一条普通的 TCP 隧道承载。

<!-- TOC -->

- [rathole](#rathole)
  - [Features](#features)
  - [Quickstart](#quickstart)
  - [Configuration](#configuration)
    - [Routing](#routing)
    - [Logging](#logging)
    - [Tuning](#tuning)
  - [Benchmark](#benchmark)
  - [Development Status](#development-status)

<!-- /TOC -->

## Features

- **HTTP 反向代理** 一个公网 HTTP 端口即可服务所有服务，按 `Host` 头路由。WebSocket 及其他协议升级可原样透传。
- **高性能** 具有更高的吞吐量，高并发下更稳定。见[Benchmark](#benchmark)
- **低资源消耗** 内存占用远低于同类工具。见[Benchmark](#benchmark)
- **安全性** 每个服务单独强制鉴权。Server 和 Client 负责各自的配置。

## Quickstart

一个全功能的 `rathole` 可以从 [release](https://github.com/rapiz1/rathole/releases) 页面下载。或者 [从源码编译](docs/build-guide.md) **获取其他平台和最小化的二进制文件**。

使用 rathole 需要一个有公网 IP 的服务器，和一个在 NAT 或防火墙后的设备，其中有些 HTTP 服务需要暴露在互联网上。

假设你在家里的 NAT 后面有一个 NAS，并且想把它的 Web 界面暴露在 `nas.example.com`：

1. 在有一个公网 IP 的服务器上

创建 `server.toml`，内容如下，并根据你的需要调整。

```toml
# server.toml
[server]
bind_addr = "0.0.0.0:2333" # `2333` 配置了服务端监听客户端连接的端口
http_bind_addr = "0.0.0.0:80" # `80` 配置了供访问者连接的 HTTP 入口
default_token = "use_a_secret_that_only_you_know"

[server.services.my_nas]
hosts = ["nas.example.com"] # 具有该 `Host` 的请求会被转发到 `my_nas`
```

然后运行:

```bash
./rathole server.toml
```

2. 在 NAT 后面的主机（你的 NAS）上

创建 `client.toml`，内容如下，并根据你的需要进行调整。

```toml
# client.toml
[client]
remote_addr = "myserver.com:2333" # 服务器的地址。端口必须与 `server.bind_addr` 中的端口相同。
default_token = "use_a_secret_that_only_you_know"

[client.services.my_nas]
local_addr = "127.0.0.1:80" # 需要被转发的本地 HTTP 服务的地址
```

然后运行：

```bash
./rathole client.toml
```

3. 现在 `rathole` 客户端会连接运行在 `myserver.com:2333`的 `rathole` 服务器，任何到服务器 `80` 端口、`Host: nas.example.com` 的 HTTP 请求将被转发到客户端所在主机的 `80` 端口。

所以你可以在 `nas.example.com` 解析到你的服务器后，访问 `http://nas.example.com`。

[Systemd examples](./examples/systemd) 中提供了一些让 `rathole` 在 Linux 上作为后台服务运行的配置示例。

## Configuration

如果只有一个 `[server]` 和 `[client]` 块存在的话，`rathole` 可以根据配置文件的内容自动决定在服务器模式或客户端模式下运行，就像 [Quickstart](#quickstart) 中的例子。

但 `[client]` 和 `[server]` 块也可以放在一个文件中。然后在服务器端，运行 `rathole --server config.toml`。在客户端，运行 `rathole --client config.toml` 来明确告诉 `rathole` 运行模式。

**推荐首先查看 [examples](./examples) 中的配置示例来快速理解配置格式**，如果有不清楚的地方再查阅完整配置格式。

下面是完整的配置格式。

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

服务端在 `http_bind_addr` 上接受 HTTP 连接。对每个连接，它读取 HTTP 请求行与 `Host` 头（不触碰请求体），然后把该连接转发给 `hosts` 中包含该 host 的服务。Host 匹配不区分大小写，并忽略端口。

如果没有服务匹配该 `Host`，服务端返回 `404`。如果匹配到的服务尚未连接，则返回 `503`。

路由对每条连接只进行一次，基于其第一个请求。若后续请求在 keep-alive 连接上携带不同的 `Host`，不会重新路由。

### Logging

`rathole`，像许多其他 Rust 程序一样，使用环境变量来控制日志级别。

支持的 Logging Level 有 `info`, `warn`, `error`, `debug`, `trace`

比如将日志级别设置为 `error`:

```shell
RUST_LOG=error ./rathole config.toml
```

如果 `RUST_LOG` 不存在，默认的日志级别是 `info`。

### Tuning

rathole 默认启用 TCP_NODELAY。这能够减少延迟并使交互式应用受益。但它会减少一些带宽。

如果带宽更重要，TCP_NODELAY 仍然可以通过配置 `nodelay = false` 关闭（按服务配置）。

## Benchmark

rathole 的延迟与 [frp](https://github.com/fatedier/frp) 相近，在高并发情况下表现更好，能提供更大的带宽，内存占用更少。

关于测试进行的更多细节，参见单独页面 [Benchmark](./docs/benchmark.md)。

![http_throughput](./docs/img/http_throughput.svg)
![tcp_bitrate](./docs/img/tcp_bitrate.svg)
![mem](./docs/img/mem-graph.png)

## Development Status

`rathole` 正在积极开发中

- [x] HTTP 反向代理
- [ ] 用于配置的 HTTP APIs

[Out of Scope](./docs/out-of-scope.md) 列举了没有计划开发的特性并说明了原因。
