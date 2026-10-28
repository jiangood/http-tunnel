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
- 可选的多跳中继（`htn`）：一个进程一跳，无需每跳部署 `hts` + `htc` 两套进程

> **不兼容变更（v0.21.0）：** TCP 传输已移除，旧配置中的 `transport = "tcp"` 以及 `hts.toml` 中旧的 TCP 监听端口都不再需要；请改用 WebSocket 配置（见下文迁移说明）。服务端的 `control_host` 已改名为 `server_host`，`http_port` 已改名为 `server_port`。
>
> **配置简化：** 客户端的连接信息合并为一行 `server = "wss://control.example.com:443"`。旧的客户端 `transport`、`server_host`、`server_port` 字段不再支持，启动时会提示迁移（与上面 `hts.toml` 的新字段只是同名，含义不同）。

## 快速开始

从 [Releases](https://github.com/jiangood/http-tunnel/releases) 下载 `hts`（服务端）、`htc`（客户端），需要多跳中继时再下载 `htn`（节点）。

### 1. 配置服务端

首次运行会把配置模板写入 `hts.toml`，然后退出：

```sh
./hts
```

用 `openssl rand -hex 16` 生成一个强 `token` 填入模板，并填写服务域名：

```toml
server_port = 80
token = "把 openssl rand -hex 16 的输出填在这里" # 必须与客户端 htc.toml 的 token 相同
# 必填：客户端专用的服务域名，必须解析到本机
server_host = "control.example.com"
# 可选：监听地址，默认 0.0.0.0（所有网卡）；放在本机反向代理后面时可改成 127.0.0.1
# bind_host = "127.0.0.1"
```

可选的 `[[proxies]]` 可把域名直接转发到服务端本机的服务，见[转发配置](#转发配置)。

再次运行 `./hts` 启动服务端。服务端只监听 `server_port`，控制连接和业务请求共用这个端口。`bind_host` 决定绑定哪个地址：默认 `0.0.0.0` 监听所有网卡，设为 `127.0.0.1` 时只接受本机连接（适合由同机反向代理终止 TLS 的部署）。

`server_host` 是流量的分流键：命中该 Host 且路径为 `/_http-tunnel/v1/connect` 的请求会走隧道协议，其余 Host 仍按业务域名路由。它必须是一个精确域名（不能是泛域名），并且不能与任何 `[[proxies]]` 或客户端隧道域名重叠。

### 2. 配置客户端

首次运行会把配置模板写入 `htc.toml`：

```sh
./htc
```

编辑配置，填写服务端地址、与服务端相同的 `token` 和转发规则：

```toml
server = "wss://control.example.com:443" # 协议 + 服务端 server_host + server_port
name = "home"
token = "与服务端相同的密钥"
web_port = 8610

[[tunnels]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
```

再次运行 `./htc` 启动客户端。之后访问 `https://nas.example.com`，请求会转发到客户端的本地服务。

> `server` 的端口可以省略：`wss` 默认 443，`ws` 默认 80。旧配置中的 `remote = "host:port"` 已移除；客户端的 `transport`、`server_host` 和 `server_port` 也不再支持，请改用 `server`（服务端 `hts.toml` 的同名字段是另一回事）。

## 用环境变量配置客户端

`htc` 的每个配置项都有对应的 `HTC_*` 环境变量，环境变量优先于 `htc.toml`，适合 Docker 或不便挂载配置文件的场景：

| 环境变量 | 对应 `htc.toml` | 说明 |
| --- | --- | --- |
| `HTC_SERVER` | `server` | `ws://` 或 `wss://` 地址 |
| `HTC_NAME` | `name` | 客户端名称 |
| `HTC_TOKEN` | `token` | 与服务端相同的共享密钥 |
| `HTC_WEB_PORT` | `web_port` | 管理页端口，`0` 关闭 |
| `HTC_WEB_HOST` | `web_host` | 管理页监听地址，默认 `0.0.0.0` |
| `HTC_HEARTBEAT_TIMEOUT` | `heartbeat_timeout` | 心跳超时秒数 |
| `HTC_KEEPALIVE_INTERVAL` | `keepalive_interval` | WebSocket ping 间隔秒数 |
| `HTC_RETRY_INTERVAL` | `retry_interval` | 重连间隔秒数 |
| `HTC_TUNNELS` | `[[tunnels]]` | TOML 内联数组 |

`HTC_TUNNELS` 的每个元素支持 `name`、`domain`、`local_addr` 和可选的 `auth_header`：

```sh
export HTC_SERVER=wss://control.example.com:443
export HTC_NAME=home
export HTC_TOKEN=与服务端相同的密钥
export HTC_WEB_PORT=0
export HTC_TUNNELS='[
  {name="nas", domain="nas.example.com", local_addr="127.0.0.1:80"},
  {name="docker", domain="docker.example.com", local_addr="unix:///var/run/docker.sock", auth_header="另一个密钥"}
]'
./htc
```

规则：

- 未设置或值为空字符串的变量会被忽略，方便 compose 里写 `HTC_TOKEN=${HTC_TOKEN:-}`。
- `HTC_TUNNELS` 一旦设置就整体替换文件中的 `[[tunnels]]`，不会合并；`HTC_TUNNELS='[]'` 表示不启用任何隧道。
- 环境变量只影响运行时配置，不会写回 `htc.toml`。
- 隧道来自 `HTC_TUNNELS` 时，Web 管理页和 `PUT`/`DELETE /api/tunnels/*` 都不能修改隧道（返回 `409`）；修改环境变量并重启 `htc` 后才会生效。
- `htc.toml` 不存在时，只要设置了任一 `HTC_*`，客户端就用默认值加环境变量启动，不再生成示例文件。

## 传输模式与 TLS

隧道只支持 WebSocket，`server` 的 scheme 决定是否使用 TLS：

| `server` 示例 | 含义 |
| --- | --- |
| `wss://control.example.com`（推荐） | TLS WebSocket，默认端口 443，公网部署必须使用 |
| `ws://127.0.0.1:8080` | 明文 WebSocket，默认端口 80，仅适合可信网络或本地测试 |

`wss` 依赖可信的 TLS 终止层（Nginx、Caddy、云负载均衡或安全网关）提供服务端证书。客户端会校验服务端证书，证书的 `subjectAltName` 必须包含 `server` 中的主机名。服务端本身不申请证书，也不监听明文以外的端口。

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

**安全提示：** 明文 `ws://` 下 Token、控制消息和隧道数据都可能被网络中间方读取或篡改。公网部署必须让服务端 HTTP 入口走 TLS，并在客户端使用 `wss://`。`server_host` 只是路由配置，不是身份认证；服务端仍会校验共享 Token。

### 从 TCP 迁移

1. 在 `hts.toml` 中不再需要旧的 TCP 监听端口；该文件的 `control_host` 已改名为 `server_host`（指向服务端的一个域名），`http_port` 已改名为 `server_port`。旧字段名启动时会给出重命名提示。
2. 在 `htc.toml` 中用一行 `server = "wss://control.example.com:443"` 代替旧的客户端字段 `transport`、`server_host` 和 `server_port`；主机填服务端 `server_host`，端口填服务端 `server_port`。这三个旧客户端字段已不再支持，启动时会给出迁移提示。
3. 删除旧的 2333 端口映射/防火墙放行规则；只需保留 HTTP 入口端口。
4. 为 `server_host` 配置 TLS 证书（或让反向代理终止 TLS）。

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

## 多跳中继（node 模式）

三个以上互相隔离的网段里，只有一段能访问公网入口时，可以在每个中转走 `htn`。一个节点就是一个进程：它监听访客和下游节点，把下游的域名发布到上游，也可以直接提供本地 `[[proxies]]`。

```toml
# 中转 B 的 htn.toml
name = "relay-b"
server_port = 9001              # 必填，不能为 0；访客、下游节点和 WebSocket 共用这个端口
server_host = "b.lan"           # 下游节点连接时使用的 Host
# bind_host = "0.0.0.0"         # 可选，默认 0.0.0.0；必须是 0.0.0.0 或回环地址，且要覆盖下游/访客的来源接口
token = "下游节点使用的密钥"

[upstream]
server = "ws://a.lan:9000"      # 上一跳，跨公网时用 wss://
token = "上一跳的密钥"
mirror = true                   # 推荐：每个下游域名单独向上游认领
# claim = "*.b.relay.example.com" # 或者只认领一个精确/泛域名（更省连接，但上层看不到真实域名）

[[proxies]]                     # 可选：本节点直接提供的服务
name = "local"
domain = "local.b.relay.example.com"
local_addr = "127.0.0.1:8080"
```

- 没有 `[upstream]` 的节点就是公网入口，行为与 `hts` 相同。
- `mirror = true` 与 `claim` 二选一：前者随下游路由动态增删上游 control channel，后者只认领一个精确/泛域名。
- 每个节点只监听一个端口（`server_port`），`server_host` 是该端口上保留给对等连接的 Host；`bind_host` 必须是 `0.0.0.0` 或回环地址，因为上游隧道会把访客转发到 `127.0.0.1:<server_port>`。
- 数据面是递归的：上游隧道把访客转发到本节点的 `127.0.0.1:<server_port>`，本节点再按 `Host` 路由一次。`Host` 和报文不做任何改写，因此 WebSocket 升级同样可以穿过整条链。
- 关闭时会先撤销上游路由，再 drain 进行中的访客，最后断开下游控制通道。
- 每个节点用不同的 token；`claim` 不要与 `server_host` 重叠，指向自身监听地址的 `[upstream]` 会在启动时被拒绝。
- 镜像模式下每个域名占 1 条 control channel + 上游 `pool_min` 条预热 data channel；域名较多时在中继上设 `pool_min = 1`，并注意 `max_control_connections`（默认 256）与 `max_mirrored_routes`（默认 256，`0` 关闭上限）会限制单跳可镜像的域名数。

完整的配置字段、校验规则、失败语义和示例见 [docs/node-mode.md](./docs/node-mode.md)。

## 客户端 Web 管理

客户端管理页面和原有 API 共用 `web_port` 端口，默认 `8610`；设置 `web_port = 0` 可关闭。监听地址由 `web_host` 决定，默认 `0.0.0.0`（所有网卡），设为 `127.0.0.1` 时只能从本机访问。浏览器打开 `http://客户端地址:8610/`，登录时用户名填写 `htc`，密码填写客户端配置中的 `token`。页面使用原生 HTML 表单，不包含 CSS 或 JavaScript。

原有 API 路径和 Bearer Token 认证保持不变：

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/api/status` | 查看客户端状态 |
| GET | `/api/tunnels` | 列出隧道 |
| PUT | `/api/tunnels/{name}` | 新增或更新隧道 |
| DELETE | `/api/tunnels/{name}` | 删除隧道 |

管理端口默认绑定到 `0.0.0.0`，请勿直接暴露在公网；建议通过防火墙或私有网络限制访问，或设置 `web_host = "127.0.0.1"` 只允许本机访问。

## Docker

服务端、客户端和中继节点使用独立镜像：`ghcr.io/jiangood/hts:latest`（服务端）、`ghcr.io/jiangood/htc:latest`（客户端）和 `ghcr.io/jiangood/htn:latest`（节点），目前仅发布 `linux/amd64` 架构，其他架构请参考 [build-guide.md](./docs/build-guide.md) 自行构建。生产环境建议把 `:latest` 换成具体版本标签（例如 `:v0.21.0`），避免自动升级带来意外变更。

三个程序都固定读取工作目录 `/app` 下的 `hts.toml` / `htc.toml` / `htn.toml`，所以通常只需要把宿主机上的配置目录挂载到容器的 `/app`。客户端也可以完全用 `HTC_*` 环境变量配置，此时不需要挂载配置文件。

### 1. 准备配置

在宿主机上创建配置目录，并让容器首次启动把配置模板写入 `hts.toml`（写入后退出）：

```sh
mkdir -p /opt/hts /opt/htc
docker run --rm -v /opt/hts:/app ghcr.io/jiangood/hts:latest
```

在 `hts.toml` 中填写 `server_host` 和 `token`（可用 `openssl rand -hex 16` 生成），然后把同一个 `token` 填入 `htc.toml`，并按 [快速开始](#快速开始) 的说明修改 `server`、`[[tunnels]]` 等字段。

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

也可以不挂载 `htc.toml`，改用环境变量（见 [用环境变量配置客户端](#用环境变量配置客户端)）：

```sh
docker run -d --name htc --restart unless-stopped \
  -e HTC_SERVER=wss://control.example.com:443 \
  -e HTC_NAME=home \
  -e HTC_TOKEN=与服务端相同的密钥 \
  -e HTC_WEB_PORT=0 \
  -e HTC_TUNNELS='[{name="nas",domain="nas.example.com",local_addr="127.0.0.1:80"}]' \
  ghcr.io/jiangood/htc:latest
```

### 3. docker-compose

服务端 `docker-compose.yml`（部署在公网服务器）：

```yaml
services:
  hts:
    image: ghcr.io/jiangood/hts:latest
    container_name: hts
    restart: unless-stopped
    ports:
      - "80:80"       # HTTP 入口，需与 hts.toml 的 server_port 一致
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

如果只是在一台机器上试用，可以把两段合并到同一个 `docker-compose.yml`，把 `htc.toml` 的 `server` 写成 `ws://hts:server_port`（`hts` 是 compose 服务名），两者仍分别挂载各自的配置目录。

注意事项：

- 端口映射必须与配置保持一致：服务端只有 `server_port`（默认 `80`），客户端只有 `web_port`（默认 `8610`）。
- 客户端配置中的 `local_addr` 在客户端容器内解析：`127.0.0.1:80` 指的是客户端容器本身，不是宿主机。转发宿主机服务请用 `host.docker.internal:<端口>`（Linux 需 `extra_hosts`）或宿主机的内网 IP，转发同一 compose 中的容器用服务名。
- 客户端不需要任何入站端口（`web_port` 除外），服务端只需要 `server_port`。
- 容器内的 `bind_host` / `web_host` 保持默认 `0.0.0.0`，否则 Docker 的端口映射无法把流量转发进容器；只有宿主机上的进程才适合改成 `127.0.0.1`。
- 日志输出到容器标准输出，使用 `docker compose logs -f hts` 或 `docker logs htc` 查看，不需要额外挂载日志文件。

## 许可证

[Apache-2.0](./LICENSE)
