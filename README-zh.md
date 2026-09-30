# rathole

![rathole-logo](./docs/img/rathole-logo.png)

[![GitHub stars](https://img.shields.io/github/stars/rapiz1/rathole)](https://github.com/rapiz1/rathole/stargazers)
[![GitHub release (latest SemVer)](https://img.shields.io/github/v/release/rapiz1/rathole)](https://github.com/rapiz1/rathole/releases)
![GitHub Workflow Status (branch)](https://img.shields.io/github/actions/workflow/status/rapiz1/rathole/rust.yml?branch=main)
[![GitHub all releases](https://img.shields.io/github/downloads/rapiz1/rathole/total)](https://github.com/rapiz1/rathole/releases)
![Docker Pulls](https://img.shields.io/docker/pulls/rapiz1/rathole)

[English](README.md) | [简体中文](README-zh.md)

安全、稳定、高性能的 HTTP 内网穿透工具，用 Rust 语言编写

`rathole` 可以将 NAT 后的设备上的 HTTP 服务通过具有公网 IP 的服务器暴露在公网上。服务端只监听一个 HTTP 端口，并根据请求的 `Host` 头把请求路由到对应的服务。服务端与客户端之间的流量通过一条普通的 TCP 隧道承载。

所有的配置都放在服务端。客户端由服务端配置：客户端不需要配置文件，服务端会把需要转发的服务下发给它。

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

- **HTTP 反向代理** 一个公网 HTTP 端口即可服务所有服务，按 `Host` 头路由。WebSocket 及其他协议升级可原样透传。
- **只在服务端配置** 只有服务端需要配置。客户端只需给出服务端地址、自己的名字和 token，服务端会把要转发的服务下发给它。
- **高性能** 具有更高的吞吐量，高并发下更稳定。见[Benchmark](#benchmark)
- **低资源消耗** 内存占用远低于同类工具。见[Benchmark](#benchmark)
- **安全性** 每个客户端用自己的 token 鉴权，且只能代理服务端分配给它的服务。

## Quickstart

一个全功能的 `rathole` 可以从 [release](https://github.com/rapiz1/rathole/releases) 页面下载。或者 [从源码编译](docs/build-guide.md) **获取其他平台和最小化的二进制文件**。

使用 rathole 需要一个有公网 IP 的服务器，和一个在 NAT 或防火墙后的设备，其中有些 HTTP 服务需要暴露在互联网上。

假设你在家里的 NAT 后面有一个 NAS，并且想把它的 Web 界面暴露在 `nas.example.com`：

1. 在有一个公网 IP 的服务器上

创建 `server.toml`，内容如下，并根据你的需要调整。

```toml
# server.toml
bind_addr = "0.0.0.0:2333" # `2333` 配置了服务端监听客户端连接的端口
http_bind_addr = "0.0.0.0:80" # `80` 配置了供访问者连接的 HTTP 入口

[clients.home_nas] # 客户端的名字
token = "use_a_secret_that_only_you_know" # 客户端的 token

[clients.home_nas.services.my_nas]
hosts = ["nas.example.com"] # 具有该 `Host` 的请求会被转发到 `my_nas`
local_addr = "127.0.0.1:80" # NAS 上的 Web 界面地址，从 NAS 的角度看
```

然后运行:

```bash
./rathole server server.toml
```

2. 在 NAT 后面的主机（你的 NAS）上

客户端不需要配置文件。只需要告诉它服务端的地址、它在服务端配置中的名字，以及它的 token：

```bash
./rathole client --remote myserver.com:2333 --name home_nas --token use_a_secret_that_only_you_know
```

或者用环境变量传入 token，这样它就不会出现在 `ps` 里：

```bash
RATHOLE_TOKEN=use_a_secret_that_only_you_know ./rathole client --remote myserver.com:2333 --name home_nas
```

3. 现在 `rathole` 客户端会连接运行在 `myserver.com:2333` 的 `rathole` 服务器，服务端会把 `home_nas` 的服务（包括
   `my_nas`）下发给它。任何到服务器 `80` 端口、`Host: nas.example.com` 的 HTTP 请求将被转发到客户端所在主机的 `80`
   端口。

所以你可以在 `nas.example.com` 解析到你的服务器后，访问 `http://nas.example.com`。

[Systemd examples](./examples/systemd) 中提供了一些让 `rathole` 在 Linux 上作为后台服务运行的配置示例。

## Configuration

所有的配置都在一个文件里，而且它是服务端的配置。服务按「服务它的客户端」分组，每个客户端由一个名字标识，并用各自的
token 鉴权。

**推荐首先查看 [examples](./examples) 中的配置示例来快速理解配置格式**，如果有不清楚的地方再查阅完整配置格式。

下面是完整的配置格式。

```toml
bind_addr = "0.0.0.0:2333" # Necessary. The address that the server listens for clients. Generally only the port needs to be change.
http_bind_addr = "0.0.0.0:80" # Necessary. The HTTP entrypoint. Visitors are routed by the `Host` header
heartbeat_interval = 30 # Optional. The interval between two application-layer heartbeat. Set to 0 to disable sending heartbeat. Default: 30 seconds

[clients.home] # 一个客户端。名字 `home` 必须和客户端的 `--name` 一致
token = "use_a_secret_that_only_you_know" # Necessary. 客户端的 token，也可以通过 `RATHOLE_TOKEN` 环境变量传入
heartbeat_timeout = 40 # Optional. Set to 0 to disable the application-layer heartbeat test. The value must be greater than `heartbeat_interval`. Default: 40 seconds
retry_interval = 1 # Optional. 客户端连接服务端的重试间隔。Default: 1 second
nodelay = true # Optional. 该客户端下所有服务默认是否启用 TCP_NODELAY。Default: true

[clients.home.services.service1] # `home` 的一个服务，名字 `service1` 可以任意取
hosts = ["service1.example.com"] # Necessary. 具有这些 `Host` 的请求会被路由到该服务
local_addr = "127.0.0.1:1081" # Necessary. 该服务在客户端侧的本地地址
nodelay = true # Optional. 是否启用 TCP_NODELAY。Default: 继承该客户端的配置
retry_interval = 1 # Optional. 连接服务端的重试间隔。Default: 继承该客户端的配置

[clients.home.services.service2] # 可以定义多个服务
hosts = ["service2.example.com", "www.service2.example.com"]
local_addr = "127.0.0.1:1082"

[clients.office] # 可以定义多个客户端，各自用 `--name office` 启动
token = "another_secret"
nodelay = false # 除非服务单独覆盖，否则作用于 `office` 的所有服务

[clients.office.services.service3]
hosts = ["service3.example.com"]
local_addr = "127.0.0.1:1083"
```

服务名是全局的：两个客户端不能定义同名的服务，同一个 `Host` 也不能被两个服务占用。修改配置后需要重启服务端和客户端
才能生效，客户端只在启动时拉取一次配置。

### Routing

服务端在 `http_bind_addr` 上接受 HTTP 连接。对每个连接，它读取 HTTP 请求行与 `Host` 头（不触碰请求体），然后把该连接转发给 `hosts` 中包含该 host 的服务。Host 匹配不区分大小写，并忽略端口。

如果没有服务匹配该 `Host`，服务端返回 `404`。如果匹配到的服务尚未连接，则返回 `503`。

路由对每条连接只进行一次，基于其第一个请求。若后续请求在 keep-alive 连接上携带不同的 `Host`，不会重新路由。

### Logging

`rathole`，像许多其他 Rust 程序一样，使用环境变量来控制日志级别。

支持的 Logging Level 有 `info`, `warn`, `error`, `debug`, `trace`

比如将日志级别设置为 `error`:

```shell
RUST_LOG=error ./rathole server config.toml
```

如果 `RUST_LOG` 不存在，默认的日志级别是 `info`。

### Tuning

rathole 默认启用 TCP_NODELAY。这能够减少延迟并使交互式应用受益。但它会减少一些带宽。

如果带宽更重要，TCP_NODELAY 仍然可以通过配置 `nodelay = false` 关闭（按客户端或按服务配置）。

## Benchmark

rathole 的延迟与 [frp](https://github.com/fatedier/frp) 相近，在高并发情况下表现更好，能提供更大的带宽，内存占用更少。

关于测试进行的更多细节，参见单独页面 [Benchmark](./docs/benchmark.md)。

![http_throughput](./docs/img/http_throughput.svg)
![tcp_bitrate](./docs/img/tcp_bitrate.svg)
![mem](./docs/img/mem-graph.png)

## Planning

- [ ] 用于配置的 HTTP APIs

[Out of Scope](./docs/out-of-scope.md) 列举了没有计划开发的特性并说明了原因。
