# http-tunnel

[![Release](https://img.shields.io/github/v/release/jiangood/http-tunnel)](https://github.com/jiangood/http-tunnel/releases)
[![Build](https://img.shields.io/github/actions/workflow/status/jiangood/http-tunnel/rust-checks.yml?branch=main)](https://github.com/jiangood/http-tunnel/actions)
[![License](https://img.shields.io/github/license/jiangood/http-tunnel)](./LICENSE)

**http-tunnel** 是一个基于 Rust 的 HTTP 反向隧道工具，可将内网服务通过公网服务器对外提供访问。服务端根据 HTTP `Host` 头路由请求，客户端负责声明转发规则。

## 功能

- 多域名和泛域名路由
- 支持 WebSocket 和 HTTP 协议升级，WebSocket 传输支持 `wss://`（TLS）
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
server_host = "your-server.example.com"
server_port = 2333
name = "home"
token = "与服务端相同的密钥"
web_port = 8610

[[tunnels]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
```

`server_host` 和 `server_port` 必须同时填写。默认的 TCP 模式下，`server_port` 就是服务端 `hts.toml` 的 `server_port`。

> 旧配置中的 `remote = "host:port"` 已移除；启动时会提示改用 `server_host` 和 `server_port`。

再次运行 `./htc` 启动客户端。之后访问 `http://nas.example.com`，请求会转发到客户端的本地服务。

## WebSocket 传输模式（实验性）

默认客户端仍使用原有 TCP 传输（服务端默认 2333 端口）。如果客户端无法连接该 TCP 端口，可以选择通过服务端 HTTP 入口建立 WebSocket 连接；服务端和客户端均不需要新增公网端口，也不需要 Nginx。

在 hts.toml 中为控制连接预留一个独立域名：

```toml
server_port = 2333
http_port = 80
control_host = "control.example.com"
token = "与客户端相同的随机强密钥"
```

确保 control.example.com 的 DNS 记录指向服务端。在 htc.toml 中，把 `server_host` 设为该控制域名，`server_port` 设为服务端 HTTP 入口端口（`http_port`，不是 `server_port`），并启用 WebSocket：

```toml
server_host = "control.example.com"
server_port = 80
transport = "ws"
name = "home"
token = "与服务端相同的随机强密钥"

[[tunnels]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
```

> 注意：WebSocket 模式下 `server_port` 填的是服务端的 `http_port`（上例为 `80`），`server_host` 必须与服务端的 `control_host` 一致。启动日志会明确打印当前使用的传输模式和连接地址。

如果服务端 HTTP 入口是 HTTPS（例如由 Nginx、云负载均衡或安全网关终止 TLS），把 `transport` 写成 `wss` 即可（任何端口），客户端会完成 TLS 握手并校验服务端证书：

```toml
server_host = "control.example.com"
server_port = 443
transport = "wss"          # 使用 wss://
```

`transport` 的取值：

| 值 | 含义 |
| --- | --- |
| `tcp`（默认） | 原始 TCP 传输，连服务端 `server_port` |
| `ws` | WebSocket 传输，明文 `ws://` |
| `wss` | WebSocket 传输，TLS `wss://` |

协议只由 `transport` 决定，不再根据端口自动推断。旧版本写的 `transport = "websocket"` 仍可用，等价于 `ws`。

TLS 证书的 `subjectAltName` 必须包含 `server_host`，否则握手会因证书校验失败而报错。

控制域名仅用于内部控制与数据连接；普通业务域名（例如 nas.example.com）继续通过 http_port 进入现有 Host 路由。服务端会根据 Host 和专用路径 /_http-tunnel/v1/connect 识别 WebSocket 连接，并直接交给隧道协议处理器。原来的 2333/TCP 模式不受影响。

如果服务端完全不需要 TCP 端口，**在 `hts.toml` 中不写 `server_port` 这一行**即可关闭该监听；此时 `control_host` 必须配置，否则服务端没有客户端入口，启动时会直接报错。注意默认模板里带着 `server_port = 2333`，要关闭就得把它删掉（写成 `0` 会被拒绝）：

```toml
# hts.toml 中不写 server_port，即不监听 TCP，只接受 WebSocket 控制连接
http_port = 80
control_host = "control.example.com"
token = "与客户端相同的随机强密钥"
```

WebSocket 连接不再经过本地 2333 回环桥接，因此进程也不需要占用任何本地 TCP 端口（除了 `http_port`）。

**安全提示：** 明文 `ws://` 下 Token、控制消息和隧道数据可能被网络中间方读取或篡改，仅适合可信网络或测试环境。公网部署应让服务端 HTTP 入口走 TLS（由 Nginx、云负载均衡或安全网关终止证书），客户端使用 `transport = "wss"`（或将 `server_port` 设为 `443`），此时客户端使用 `wss://` 并校验服务端证书，链路由 TLS 保护。**注意：不要在入口上开启“HTTP 强制跳转 HTTPS”**——`ws://` 客户端不会跟随 301/308 重定向，隧道会直接连不上；要启用 HTTPS 就把客户端也切到 `wss://`。`control_host` 只是路由配置，不是身份认证；服务端仍会验证共享 Token。

资源保护：单个 WebSocket 帧/消息最大 1 MiB，服务端最多允许 256 条活动 WebSocket 连接（控制通道与数据通道合计）；HTTP WebSocket 握手最长等待 10 秒，隧道协议 ClientHello 读取最长等待 5 秒。达到连接上限时，新连接会收到 503。当前上限是实验性版本的固定值，尚未提供配置项。

该模式目前是实验性功能，部署前请通过测试环境验证长连接、代理兼容性和断线重连行为。

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

服务端和客户端使用独立镜像：`ghcr.io/jiangood/hts:latest`（服务端）和 `ghcr.io/jiangood/htc:latest`（客户端），目前仅发布 `linux/amd64` 架构，其他架构请参考 [build-guide.md](./docs/build-guide.md) 自行构建。生产环境建议把 `:latest` 换成具体版本标签（例如 `:v1.2.3`），避免自动升级带来意外变更。

两个程序都固定读取工作目录 `/app` 下的 `hts.toml` / `htc.toml`，所以只需要把宿主机上的配置目录挂载到容器的 `/app`。

### 1. 准备配置

在宿主机上创建配置目录，并让容器首次启动生成 `hts.toml`（服务端会自动生成随机 `token`，写入配置后退出）：

```sh
mkdir -p /opt/hts /opt/htc
docker run --rm -v /opt/hts:/app ghcr.io/jiangood/hts:latest
```

把输出中的 `token` 填入 `htc.toml`。客户端配置可以先 `docker run --rm -v /opt/htc:/app ghcr.io/jiangood/htc:latest` 生成模板，再按 [快速开始](#快速开始) 的说明修改 `server_host`、`server_port`、`token`、`[[tunnels]]` 等字段。

### 2. docker run

启动服务端：

```sh
docker run -d --name hts --restart unless-stopped   -p 2333:2333 -p 80:80   -v "/opt/hts:/app/"   ghcr.io/jiangood/hts:latest
```

启动客户端：

```sh
docker run -d --name htc --restart unless-stopped   -v "/opt/htc:/app/"   ghcr.io/jiangood/htc:latest
```

如果需要代理 unix sock，例如docker api，还需要 参数 `-v /var/run/docker.sock:/var/run/docker.sock`

### 3. docker-compose

服务端 `docker-compose.yml`（部署在公网服务器）：

```yaml
services:
  hts:
    image: ghcr.io/jiangood/hts:latest
    container_name: hts
    restart: unless-stopped
    ports:
      - "2333:2333"   # 客户端控制连接，需与 hts.toml 的 server_port 一致
      - "80:80"       # HTTP 入口，需与 hts.toml 的 http_port 一致
    volumes:
      - /opt/hts:/app
```

客户端 `docker-compose.yml`（部署在内网机器）：

```yaml
services:
  htc:
    image: ghcr.io/jiangood/htc:latest
    container_name: htc
    restart: unless-stopped
    ports:
      - "8610:8610"   # Web 管理页与 API，htc.toml 中 web_port = 0 可关闭
    volumes:
      - /opt/htc:/app
      # 需要转发 Unix Socket（例如 Docker API）时取消注释，
      # 并保证路径与 htc.toml 中 local_addr 的 unix:// 路径一致
      # - /var/run/docker.sock:/var/run/docker.sock
    # 需要转发宿主机上的服务时（Linux 需额外添加下面两行）
    # extra_hosts:
    #   - "host.docker.internal:host-gateway"
```

保存后分别执行：

```sh
docker compose up -d
docker compose logs -f      # 查看日志
```

如果只是在一台机器上试用，可以把两段合并到同一个 `docker-compose.yml`，把 `htc.toml` 的 `server_host` 写成 `hts`（compose 服务名）、`server_port` 写成 `2333`，两者仍分别挂载各自的配置目录。

注意事项：

- 端口映射必须与配置保持一致：服务端 `server_port`（默认 `2333`）、`http_port`（默认 `80`），客户端 `web_port`（默认 `8610`）。改了配置也要同步改 `docker-compose.yml`。
- 客户端配置中的 `local_addr` 在客户端容器内解析：`127.0.0.1:80` 指的是客户端容器本身，不是宿主机。转发宿主机服务请用 `host.docker.internal:<端口>`（Linux 需 `extra_hosts`）或宿主机的内网 IP，转发同一 compose 中的容器用服务名。
- 使用 WebSocket 传输模式时，客户端不需要入站端口；服务端的 `2333` 也不必对公网开放，可以删掉该端口映射，只保留 `http_port`，并把 `htc.toml` 的 `server_host` 指向控制域名（例如 `control.example.com`）、`server_port` 指向 HTTP 入口端口（例如 `80`），同时设置 `transport = "ws"`（HTTPS 入口则用 `"wss"`）。如果从 `hts.toml` 中删掉 `server_port`，服务端根本不会监听该端口，端口映射可以整条删除。
- 日志输出到容器标准输出，使用 `docker compose logs -f hts` 或 `docker logs htc` 查看，不需要额外挂载日志文件。

## 许可证

[Apache-2.0](./LICENSE)
