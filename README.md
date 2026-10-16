# http-tunnel

[![Release](https://img.shields.io/github/v/release/jiangood/http-tunnel)](https://github.com/jiangood/http-tunnel/releases)
[![Build](https://img.shields.io/github/actions/workflow/status/jiangood/http-tunnel/rust-checks.yml?branch=main)](https://github.com/jiangood/http-tunnel/actions)
[![License](https://img.shields.io/github/license/jiangood/http-tunnel)](./LICENSE)

**http-tunnel** 是一个基于 Rust 的 HTTP 反向隧道工具，可将内网服务通过公网服务器对外提供访问。服务端根据 HTTP `Host` 头路由请求，客户端负责声明转发规则。

## 功能

- 多域名和泛域名路由
- 支持 WebSocket 和 HTTP 协议升级
- 支持服务端直连代理和客户端隧道
- 支持 TCP 地址和 Unix Socket
- 可为转发规则开启入口校验，要求访客携带 `X-HTTP-TUNNEL-TOKEN`

## 快速开始

从 [Releases](https://github.com/jiangood/http-tunnel/releases) 下载 `hts`（服务端）和 `htc`（客户端）。

### 1. 配置服务端

首次运行会生成 `hts.toml`，其中 `token` 是自动生成的随机强密钥：

```sh
./hts
```

把生成的 `token` 填到 `htc.toml`，其余字段可按需调整：

```toml
server_port = 2333
http_port = 80
token = "自动生成的随机强密钥"

# 可选：将指定域名转发到服务端本机的服务
[[proxies]]
name = "grafana"
domain = "grafana.example.com"
local_addr = "127.0.0.1:3000"
```

再次运行 `./hts` 启动服务端。客户端连接端口默认为 `2333`，HTTP 访问端口默认为 `80`。

### 2. 配置客户端

首次运行会生成 `htc.toml`：

```sh
./htc
```

编辑配置，填写服务端地址、与服务端相同的 `token` 和转发规则：

```toml
remote = "your-server.example.com:2333"
name = "home"
token = "与服务端相同的密钥"
web_port = 8610

[[tunnels]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
```

再次运行 `./htc` 启动客户端。之后访问 `http://nas.example.com`，请求会转发到客户端的本地服务。

## 转发配置

客户端使用 `[[tunnels]]`，服务端直连代理使用 `[[proxies]]`。两者均支持以下字段：

| 字段 | 说明 |
| --- | --- |
| `name` | 规则名称 |
| `domain` | 要匹配的域名，支持如 `*.example.com` 的泛域名 |
| `local_addr` | 本地目标地址，可使用 TCP 地址或 Unix Socket |
| `auth_header` | 可选的访问密钥；设置后访客必须携带 `X-HTTP-TUNNEL-TOKEN: <密钥>`，否则返回 `401` |

TCP 地址示例：`127.0.0.1:3000`

Unix Socket 示例：`unix:///var/run/docker.sock`

例如，为 Docker Socket 隧道开启访问校验，只有携带正确密钥的请求才会被转发：

```toml
[[tunnels]]
name = "docker-api"
domain = "docker.example.com"
local_addr = "unix:///var/run/docker.sock"
auth_header = "请替换为一个足够长的随机密钥"
```

此时访客必须发送 `X-HTTP-TUNNEL-TOKEN: 请替换为一个足够长的随机密钥`，否则会得到 `401 Unauthorized`，响应体分别为 `Missing X-HTTP-TUNNEL-TOKEN header` 或 `Invalid X-HTTP-TUNNEL-TOKEN header`，说明是没带还是值不对。校验通过后，该头会在转发给后端之前被移除，后端看不到密钥；请求里的 `Authorization` 等其他头不受影响，仍原样传给后端。

服务端的 `[[proxies]]` 也支持相同的 `local_addr` 和 `auth_header` 配置。`auth_header` 为可选字段，不设置时不校验，任何请求都会被转发。

校验用的是自定义头 `X-HTTP-TUNNEL-TOKEN`，而不是 `Authorization`，因此不会和你后端的登录鉴权冲突。

**安全提示：** `auth_header` 是隧道的入口校验，密钥一旦泄露即可绕过。请使用足够长的随机值，并尽量配合 HTTPS 使用；浏览器和 `curl` 都能直接发送这个头。

## 客户端 Web 管理

客户端管理页面和原有 API 共用 `web_port` 端口，默认 `8610`；设置 `web_port = 0` 可关闭。浏览器打开 `http://客户端地址:8610/`，登录时用户名填写 `htc`，密码填写客户端配置中的 `token`。页面使用原生 HTML 表单，不包含 CSS 或 JavaScript。

原有 API 路径和 Bearer Token 认证保持不变：

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/api/status` | 查看客户端状态 |
| GET | `/api/tunnels` | 列出隧道 |
| PUT | `/api/tunnels/{name}` | 新增或更新隧道 |
| DELETE | `/api/tunnels/{name}` | 删除隧道 |

管理端口绑定到 `0.0.0.0`，请勿直接暴露在公网；建议通过防火墙或私有网络限制访问。

## Docker

服务端和客户端使用独立镜像。请先在宿主机准备好 `hts.toml` 和 `htc.toml`。

启动服务端：

```sh
docker run -d --name hts --restart unless-stopped   -p 2333:2333 -p 80:80   -v "/opt/hts:/app/"   ghcr.io/jiangood/hts:latest
```

启动客户端：

```sh
docker run -d --name htc --restart unless-stopped   -v "/opt/htc:/app/"   ghcr.io/jiangood/htc:latest
```

客户端在配置缺失或无效（例如 `token` 仍为占位符）时不会退出，而是记录错误并保持运行，修正配置后重启容器即可。

## 许可证

[Apache-2.0](./LICENSE)
