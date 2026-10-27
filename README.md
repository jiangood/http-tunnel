# http-tunnel

[![Release](https://img.shields.io/github/v/release/jiangood/http-tunnel)](https://github.com/jiangood/http-tunnel/releases)
[![Build](https://img.shields.io/github/actions/workflow/status/jiangood/http-tunnel/rust-checks.yml?branch=main)](https://github.com/jiangood/http-tunnel/actions)
[![License](https://img.shields.io/github/license/jiangood/http-tunnel)](./LICENSE)

**http-tunnel** 是一个基于 Rust 的 HTTP 反向隧道工具，可将内网服务通过公网服务器对外提供访问。服务端根据 HTTP `Host` 头路由请求，客户端负责声明转发规则。客户端与服务端之间的隧道只使用 WebSocket（`ws://` / `wss://`），服务端不需要额外的 TCP 端口。

## 功能

- 多域名和泛域名路由
- 隧道只使用 WebSocket，支持 `wss://`（TLS，默认）和 `ws://`（明文，仅限可信网络）
- 支持服务端直连代理和客户端隧道
- 支持 TCP 地址和 Unix Socket
- 可为转发规则开启入口校验，要求访客携带 `X-HTTP-TUNNEL-TOKEN`
- 控制连接与数据连接的容量分开计数，避免流量突发挤掉控制通道
- WebSocket ping/pong 保活，空闲数据通道不会被反向代理静默断开
- 握手前使用 `Authorization: Bearer` 校验共享 Token，未认证连接不占容量

> **不兼容变更（v0.21.0）：** TCP 传输已移除。旧配置中的 `transport = "tcp"` 和 `hts.toml` 中的 `server_port` 不再支持；请改用 WebSocket 配置（见下文迁移说明）。

## 快速开始

从 [Releases](https://github.com/jiangood/http-tunnel/releases) 下载 `hts`（服务端）和 `htc`（客户端）。

### 1. 配置服务端

首次运行会生成 `hts.toml`，其中 `token` 是自动生成的随机强密钥：

```sh
./hts
```

编辑配置：

```toml
http_port = 80
token = "自动生成的随机强密钥"
# 必填：客户端专用的控制域名，必须解析到本机
control_host = "control.example.com"

# 可选：将指定域名转发到服务端本机的服务
[[proxies]]
name = "grafana"
domain = "grafana.example.com"
local_addr = "127.0.0.1:3000"
```

再次运行 `./hts` 启动服务端。服务端只监听 `http_port`，控制连接和业务请求共用这个端口。

`control_host` 是流量的分流键：命中该 Host 且路径为 `/_http-tunnel/v1/connect` 的请求会走隧道协议，其余 Host 仍按业务域名路由。它必须是一个精确域名（不能是泛域名），并且不能与任何 `[[proxies]]` 或客户端隧道域名重叠。

### 2. 配置客户端

首次运行会生成 `htc.toml`：

```sh
./htc
```

编辑配置，填写控制域名、服务端 HTTP 端口、与服务端相同的 `token` 和转发规则：

```toml
transport = "wss"                 # 默认值；可信网络可用 "ws"
server_host = "control.example.com" # 服务端 hts.toml 的 control_host
server_port = 443                  # 服务端 hts.toml 的 http_port
name = "home"
token = "与服务端相同的密钥"
web_port = 8610

[[tunnels]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
```

再次运行 `./htc` 启动客户端。之后访问 `https://nas.example.com`，请求会转发到客户端的本地服务。

> 旧配置中的 `remote = "host:port"` 已移除；启动时会提示改用 `server_host` 和 `server_port`。旧版本的 `transport = "websocket"` 仍等价于 `ws`。

## 传输模式与 TLS

隧道只支持 WebSocket，`transport` 仅用于选择是否使用 TLS：

| 值 | 含义 |
| --- | --- |
| `wss`（默认） | TLS WebSocket，公网部署必须使用 |
| `ws` | 明文 WebSocket，仅适合可信网络或本地测试 |

`wss` 依赖可信的 TLS 终止层（Nginx、Caddy、云负载均衡或安全网关）提供服务端证书。客户端会校验服务端证书，证书的 `subjectAltName` 必须包含 `server_host`。服务端本身不申请证书，也不监听明文以外的端口。

Nginx 终止 TLS 的示例（`proxy_read_timeout` 必须大于客户端 `keepalive_interval`，否则空闲数据通道会被代理断开）：

```nginx
server {
    listen 443 ssl;
    server_name control.example.com;

    ssl_certificate     /etc/letsencrypt/live/control.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/control.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:80;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
        proxy_buffering off;
        proxy_read_timeout 120s;
        proxy_send_timeout 120s;
    }
}
```

**不要在入口上开启“HTTP 强制跳转 HTTPS”**：WebSocket 客户端不会跟随 301/308 重定向，隧道会直接连不上。要启用 HTTPS 就把客户端也切到 `wss://`。

**安全提示：** 明文 `ws://` 下 Token、控制消息和隧道数据都可能被网络中间方读取或篡改。公网部署必须让服务端 HTTP 入口走 TLS，并在客户端使用 `transport = "wss"`。`control_host` 只是路由配置，不是身份认证；服务端仍会校验共享 Token。

### 从 TCP 迁移

1. 在 `hts.toml` 中删除 `server_port`，添加 `control_host`（指向服务端的一个域名）。
2. 在 `htc.toml` 中设置 `transport = "wss"`（或 `ws`），把 `server_host` 改成 `control_host`，把 `server_port` 改成服务端的 `http_port`。
3. 删除旧的 2333 端口映射/防火墙放行规则；只需保留 HTTP 入口端口。
4. 为 `control_host` 配置 TLS 证书（或让反向代理终止 TLS）。

## 资源限制与保活

所有限制都可以在 `hts.toml` 中配置，默认值适合大多数部署：

```toml
# 已认证但尚未发送协议 ClientHello 的升级数
max_pending_handshakes = 256
# 控制通道上限（每个隧道 1 条）
max_control_connections = 256
# 数据通道上限（预热池 + 正在转发）
max_data_connections = 2048
# 每个隧道的预热池下限与上限
pool_min = 4
pool_max = 32
# WebSocket ping 间隔（秒），0 关闭保活
keepalive_interval = 15
```

- 控制连接与数据连接使用独立的额度，数据流量突发不会挤掉其他客户端的控制通道。
- 未认证的升级在 `Authorization: Bearer` 校验失败时直接返回 `401`，不会占用上述额度。
- 超过 `max_pending_handshakes` 时返回 `503`；握手最长等待 10 秒，协议 `ClientHello` 最长等待 5 秒。
- 单个 WebSocket 帧/消息最大 1 MiB。
- 每个 WebSocket 连接每 `keepalive_interval` 秒发送一次 ping；连续 3 个间隔没有收到任何帧就关闭连接，并从预热池中剔除，避免把已被代理断开的通道交给访客。
- 服务端仍会发送控制通道的应用层心跳（30 秒），作为保活的第二层保护。

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

服务端和客户端使用独立镜像：`ghcr.io/jiangood/hts:latest`（服务端）和 `ghcr.io/jiangood/htc:latest`（客户端），目前仅发布 `linux/amd64` 架构，其他架构请参考 [build-guide.md](./docs/build-guide.md) 自行构建。生产环境建议把 `:latest` 换成具体版本标签（例如 `:v0.21.0`），避免自动升级带来意外变更。

两个程序都固定读取工作目录 `/app` 下的 `hts.toml` / `htc.toml`，所以只需要把宿主机上的配置目录挂载到容器的 `/app`。

### 1. 准备配置

在宿主机上创建配置目录，并让容器首次启动生成 `hts.toml`（服务端会自动生成随机 `token`，写入配置后退出）：

```sh
mkdir -p /opt/hts /opt/htc
docker run --rm -v /opt/hts:/app ghcr.io/jiangood/hts:latest
```

把输出中的 `token` 填入 `htc.toml`，并按 [快速开始](#快速开始) 的说明修改 `control_host`、`http_port`、`transport`、`[[tunnels]]` 等字段。

### 2. docker run

启动服务端（只暴露 HTTP 入口；如果由 Nginx 终止 TLS，就只映射 80 到本机）：

```sh
docker run -d --name hts --restart unless-stopped   -p 80:80   -v "/opt/hts:/app/"   ghcr.io/jiangood/hts:latest
```

启动客户端：

```sh
docker run -d --name htc --restart unless-stopped   -v "/opt/htc:/app/"   ghcr.io/jiangood/htc:latest
```

如果需要代理 unix sock，例如 Docker API，还需要参数 `-v /var/run/docker.sock:/var/run/docker.sock`

### 3. docker-compose

服务端 `docker-compose.yml`（部署在公网服务器）：

```yaml
services:
  hts:
    image: ghcr.io/jiangood/hts:latest
    container_name: hts
    restart: unless-stopped
    ports:
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

如果只是在一台机器上试用，可以把两段合并到同一个 `docker-compose.yml`，把 `htc.toml` 的 `server_host` 写成 `hts`（compose 服务名）、`server_port` 写成服务端 `http_port`，两者仍分别挂载各自的配置目录。

注意事项：

- 端口映射必须与配置保持一致：服务端只有 `http_port`（默认 `80`），客户端只有 `web_port`（默认 `8610`）。
- 客户端配置中的 `local_addr` 在客户端容器内解析：`127.0.0.1:80` 指的是客户端容器本身，不是宿主机。转发宿主机服务请用 `host.docker.internal:<端口>`（Linux 需 `extra_hosts`）或宿主机的内网 IP，转发同一 compose 中的容器用服务名。
- 客户端不需要任何入站端口（`web_port` 除外），服务端只需要 `http_port`。
- 日志输出到容器标准输出，使用 `docker compose logs -f hts` 或 `docker logs htc` 查看，不需要额外挂载日志文件。

## 许可证

[Apache-2.0](./LICENSE)
