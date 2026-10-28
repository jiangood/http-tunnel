# http-tunnel

[![Release](https://img.shields.io/github/v/release/jiangood/http-tunnel)](https://github.com/jiangood/http-tunnel/releases)
[![Build](https://img.shields.io/github/actions/workflow/status/jiangood/http-tunnel/rust-checks.yml?branch=main)](https://github.com/jiangood/http-tunnel/actions)
[![License](https://img.shields.io/github/license/jiangood/http-tunnel)](./LICENSE)

**http-tunnel** 是一个基于 Rust 的 HTTP 反向隧道工具，可将内网服务通过公网服务器对外提供访问。它根据 HTTP `Host` 头路由请求，客户端与服务器之间的所有流量只使用 WebSocket（`ws://` / `wss://`）。

从本版本开始，只有一个通用程序 `ht` 和一份配置文件 `ht.toml`。这个过程**总是**运行内嵌服务器（监听端口、本地代理、下游 v4 会话），并**可选地**通过 `[upstream]` 向上一跳建立一条 v4 会话：

- 没有 `[upstream]`：独立入口（ingress/server），直接服务访客和下游对等节点。
- 有 `[upstream]`：同时充当 relay/leaf，既服务本地 `[[proxies]]`，也把路由通告给上一跳。
- 没有 `server`/`client`/`node` 模式开关，也没有三个不同的程序。

## 功能

- 多域名和泛域名路由
- 隧道只使用 WebSocket，支持 `wss://`（TLS，默认）和 `ws://`（明文，仅限可信网络）
- 本地服务统一写在 `[[proxies]]`，既可作为入口的直连代理，也可被 leaf/relay 通告到上一跳
- 支持 TCP 地址和 Unix Socket
- 可为转发规则开启入口校验，要求访客携带 `X-HTTP-TUNNEL-TOKEN`
- 控制连接与数据连接的容量分开计数，避免流量突发挤掉控制通道
- WebSocket ping/pong 保活，空闲数据通道不会被反向代理静默断开
- 握手前使用 `Authorization: Bearer` 校验共享 Token，未认证连接不占容量
- 一条 v4 会话承载所有域名和访客流；多跳时一个进程一跳
- 运行时管理页和 API 可增删本地 `[[proxies]]`，立即生效并写回 `ht.toml`

## 快速开始

从 [Releases](https://github.com/jiangood/http-tunnel/releases) 下载 `ht`。首次运行时，如果当前目录没有 `ht.toml`，程序会写入一个带注释的模板后退出：

```sh
./ht
```

编辑 `ht.toml`，至少填写 `server_host` 和强 `token`（可用 `openssl rand -hex 16` 生成）：

```toml
name = "edge"
server_port = 7777
bind_host = "0.0.0.0"
server_host = "control.example.com"
token = "把 openssl rand -hex 16 的输出填在这里"

[[proxies]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
```

再次运行 `./ht` 即可。访问 `http://nas.example.com` 的请求会被转发到这台机器的 `127.0.0.1:80`。

命令行只有以下选项：

```text
ht [-c/--config ht.toml] [-b/--background]
```

`-b/--background` 会把输出写入当前目录的 `ht.log`。运行时不写入任何模式标志：角色完全由 `ht.toml` 是否包含 `[upstream]` 决定。

## 完整配置

```toml
name = "edge"                 # 可选，默认 "ht"
server_port = 7777            # 0 = 临时端口，仅在有 [upstream] 时允许
bind_host = "0.0.0.0"         # 默认 0.0.0.0
server_host = "edge.lan"      # 可选；有 [upstream] 时默认 127.0.0.1，否则必填
token = "downstream-secret"   # 监听端口的 Token；有 [upstream] 且省略时复用 upstream token

[[proxies]]                   # 本进程直接服务的本地服务
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
# auth_header = "..."

[upstream]                    # 省略 => 独立入口
server = "wss://control.example.com:443"
token = "upstream-secret"
mirror = true                 # 未写 mirror 和 claim 时默认 true
# claim = "*.edge.example.com"
```

### 监听器与上游

- `server_port` 是唯一的 HTTP 入口端口：访客、下游对等节点、WebSocket 控制会话和管理页/API 共用它。
- `bind_host` 默认 `0.0.0.0`。有 `[upstream]` 时必须是 `0.0.0.0` 或回环地址，因为上游会话会把访客转发回本机的 `127.0.0.1:<实际 server_port>`。无上游时可以绑定任意单个 IP。
- `server_host` 是保留给 WebSocket 控制端点和管理页/API 的精确 Host，不能是泛域名，也不能与任何 `[[proxies]]` 域名重叠。无 `[upstream]` 时必填；有 `[upstream]` 时默认 `127.0.0.1`。
- `server_port = 0` 会绑定一个临时端口，并把实际端口用于上游会话的回环目标。只允许和 `[upstream]` 一起使用，否则重启后无法发现端口；此时管理页也使用这个临时端口。
- `token` 是下游对等节点使用的监听 Token。有 `[upstream]` 时可以省略，此时复用 `[upstream].token`。建议每一跳使用不同的 Token，启动时会对相同 Token 给出警告。
- `web_port` 已移除：管理页与 API 不再单独监听，而是和访客、控制会话共用 `server_port`，只在 `server_host` 上可达。浏览器打开 `http://<server_host>:<server_port>/`。

### 上游：mirror 与 claim

有 `[upstream]` 时，本进程只建立**一条** v4 会话：

- `mirror = true`（默认）：把本地 `[[proxies]]` 和下游会话声明的域名实时镜像到上一跳。上层看到真实域名集合，新增/删除路由会立即通告或撤销。
- `claim = "*.edge.example.com"`：只在上一跳声明一个精确或泛域名，上层看不到真实域名集合；更省连接，但粒度更粗。
- `mirror` 与 `claim` 只能二选一。`claim` 不能覆盖本进程的 `server_host` 或上一跳 URL 的主机名。
- 指向自身监听地址的 `[upstream]` 会被拒绝；DNS 解析回本机的检查在启动时进行。

### 环境变量

每个配置项都有 `HT_*` 环境变量，环境变量优先于 `ht.toml`。旧的 `HTC_*` 名称仍可作为回退，`HT_*` 是规范名称。

| 环境变量 | 对应 `ht.toml` | 说明 |
| --- | --- | --- |
| `HT_NAME` | `name` | 进程名称 |
| `HT_SERVER` | `[upstream].server` | `ws://` 或 `wss://` 地址；设置后自动启用上游 |
| `HT_TOKEN` | `token` | 监听端口的共享 Token |
| `HT_UPSTREAM_TOKEN` | `[upstream].token` | 上一跳的 Token（可选） |
| `HT_PROXIES` | `[[proxies]]` | TOML 内联数组 |
| `HT_SERVER_PORT` | `server_port` | 监听端口，`0` 表示临时端口 |
| `HT_BIND_HOST` | `bind_host` | 监听地址 |
| `HT_SERVER_HOST` | `server_host` | 控制端点与管理页的保留 Host |
| `HT_RETRY_INTERVAL` | `retry_interval` | 重连上一跳的间隔秒数 |
| `HT_KEEPALIVE_INTERVAL` | `keepalive_interval` | WebSocket ping 间隔秒数 |

`HT_PROXIES` 的元素支持 `name`、`domain`、`local_addr` 和可选的 `auth_header`：

```sh
export HT_SERVER=wss://control.example.com:443
export HT_TOKEN=与上一跳相同的密钥
export HT_NAME=home
export HT_PROXIES='[
  {name="nas", domain="nas.example.com", local_addr="127.0.0.1:80"},
  {name="docker", domain="docker.example.com", local_addr="unix:///var/run/docker.sock", auth_header="另一个密钥"}
]'
./ht
```

规则：

- 未设置或值为空字符串的变量会被忽略。
- `HT_PROXIES` 一旦设置就整体替换文件里的 `[[proxies]]`，不会合并；`HT_PROXIES='[]'` 表示不启用任何本地服务。
- 环境变量只影响运行时配置，不会写回 `ht.toml`。
- 本地服务来自 `HT_PROXIES` 时，网页和 `PUT`/`DELETE /api/tunnels/*` 都会被拒绝（`409`）；修改环境变量并重启 `ht` 后才会生效。
- `ht.toml` 不存在时，只要设置了任一 `HT_*`，进程就用默认值加环境变量启动，**不会**生成或写入 `ht.toml`。

## 传输模式与 TLS

`[upstream].server` 的 scheme 决定是否使用 TLS：

| 示例 | 含义 |
| --- | --- |
| `wss://control.example.com`（推荐） | TLS WebSocket，默认端口 443，公网部署必须使用 |
| `ws://127.0.0.1:7777` | 明文 WebSocket，默认端口 80，仅适合可信网络或本地测试 |

`wss` 依赖可信的 TLS 终止层（Nginx、Caddy、云负载均衡或安全网关）提供服务端证书。`ht` 自身不申请证书。Nginx 终止 TLS 的示例（`proxy_read_timeout` 必须大于 `keepalive_interval`）：

```nginx
server {
    listen 443 ssl;
    server_name control.example.com;

    ssl_certificate     /etc/letsencrypt/live/control.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/control.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:7777;
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

**不要在入口上开启“HTTP 强制跳转 HTTPS”**：WebSocket 客户端不会跟随 301/308 重定向。公网部署必须让入口走 TLS，并在上游使用 `wss://`。

## 转发配置

所有本地服务都使用 `[[proxies]]`，字段如下：

| 字段 | 说明 |
| --- | --- |
| `name` | 规则名称，进程内唯一 |
| `domain` | 要匹配的域名，支持如 `*.example.com` 的泛域名 |
| `local_addr` | 本地目标地址，可使用 TCP 地址或 Unix Socket |
| `auth_header` | 可选；设置后访客必须携带 `X-HTTP-TUNNEL-TOKEN: <密钥>`，否则返回 `401` |

TCP 示例：`127.0.0.1:3000`。Unix Socket 示例：`unix:///var/run/docker.sock`。

```toml
[[proxies]]
name = "docker-api"
domain = "docker.example.com"
local_addr = "unix:///var/run/docker.sock"
auth_header = "请替换为一个足够长的随机密钥"
```

校验通过后，`X-HTTP-TUNNEL-TOKEN` 会在转发给后端之前被移除，后端看不到密钥。校验使用自定义头而不是 `Authorization`，不会和后端自己的登录鉴权冲突。`[[tunnels]]` 仍可作为旧配置的 serde 别名接受，但新配置和文档统一使用 `[[proxies]]`。

## 多跳中继

只有一段网络能访问公网入口时，可以在每一跳运行一个 `ht`。每个进程都监听访客和下游节点，并可通过 `[upstream]` 把路由发布到上一跳。

```text
访客 -> 公网入口 ht (server_host control.example.com)
      -> relay A ht (server_host a.lan, mirror = true)
      -> relay B ht (server_host b.lan, mirror = true)
      -> leaf ht (server_port = 0, [[proxies]] 指向本地服务)
```

入口的 `ht.toml`：

```toml
name = "ingress"
server_port = 80
bind_host = "0.0.0.0"
server_host = "control.example.com"
token = "入口与下游 A 共享的密钥"
```

relay A：

```toml
name = "relay-a"
server_port = 7777
bind_host = "0.0.0.0"
server_host = "a.lan"
token = "A 与下游 B 共享的密钥"

[upstream]
server = "wss://control.example.com:443"
token = "入口与下游 A 共享的密钥"
mirror = true
```

leaf（也可以把本地服务换成 `claim` 的精简模式）：

```toml
name = "leaf"
server_port = 0
bind_host = "127.0.0.1"
server_host = "127.0.0.1"
token = "B 与 leaf 共享的密钥"

[[proxies]]
name = "nas"
domain = "nas.relay.example.com"
local_addr = "127.0.0.1:80"

[upstream]
server = "ws://b.lan:9001"
token = "B 与 leaf 共享的密钥"
mirror = true
```

- 数据面是递归的：上游会话把访客转发到本进程的 `127.0.0.1:<实际 server_port>`，本进程再按 `Host` 路由一次。`Host` 和报文不改写，因此 WebSocket 升级可以穿过整条链。
- 关闭时先关闭上游会话（撤销路由），再 drain 进行中的访客，最后关闭下游会话。
- 每个进程只监听一个端口，所有域名和访客流复用一条 v4 会话。
- `max_mirrored_routes`（默认 256，`0` 关闭上限）限制单跳可通告的域名数。

更多细节见 [docs/node-mode.md](./docs/node-mode.md)。

## 资源限制与保活

```toml
# 已认证但尚未完成 v4 握手的升级数
max_pending_handshakes = 256
# v4 会话上限（每个下游进程 1 条）
max_control_connections = 256
# WebSocket ping 间隔（秒），0 关闭保活
keepalive_interval = 15
```

- 每个下游进程只建立一条 v4 会话，所有域名和访客流都复用它。
- 未认证的升级在 `Authorization: Bearer` 校验失败时直接返回 `401`，不会占用上述额度。
- 超过 `max_pending_handshakes` 时返回 `503`；升级握手最长等待 10 秒，v4 会话握手最长等待 5 秒。
- 单个 WebSocket 帧/消息最大 1 MiB；流数据帧为 64 KiB，每个方向有 256 KiB 的初始窗口，接收方消费后发送窗口更新。
- 每个 WebSocket 连接每 `keepalive_interval` 秒发送一次 ping；连续 3 个间隔没有收到任何帧就关闭连接。

## Web 管理

管理页面与 API 始终开启，和访客、控制会话共用 `server_port`；它们只在保留的 `server_host` 上可达，其他 Host 照常走代理。浏览器打开 `http://<server_host>:<server_port>/`，HTTP Basic 用户名填 `ht`，密码填 `token`。页面使用原生 HTML 表单，不包含 CSS 或 JavaScript。若 `server_host` 不是本机可解析的地址，可用 `curl -H "Host: <server_host>" http://127.0.0.1:<server_port>/` 访问。

`server_port = 0` 时管理入口也是临时端口，以启动日志中的实际端口为准。

API 路径保持不变：

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/api/status` | 查看状态和本地服务数 |
| GET | `/api/tunnels` | 列出本地 `[[proxies]]` |
| PUT | `/api/tunnels/{name}` | 新增或更新本地服务 |
| DELETE | `/api/tunnels/{name}` | 删除本地服务 |

修改会立即更新运行中的代理列表和路由 watch，并写回 `ht.toml`。详见 [docs/client-api.md](./docs/client-api.md)。

## Docker

只发布一个通用镜像：

```sh
docker run -d --name ht --restart unless-stopped \
  --stop-timeout 40 \
  -p 7777:7777 \
  -v "/opt/ht:/app/" \
  ghcr.io/jiangood/ht:latest
```

容器固定读取工作目录 `/app` 下的 `ht.toml`。也可以完全用 `HT_*` 环境变量配置：

```sh
docker run -d --name ht --restart unless-stopped \
  --stop-timeout 40 \
  -e HT_NAME=home \
  -e HT_SERVER=wss://control.example.com:443 \
  -e HT_TOKEN=与上一跳相同的密钥 \
  -e HT_PROXIES='[{name="nas",domain="nas.example.com",local_addr="127.0.0.1:80"}]' \
  ghcr.io/jiangood/ht:latest
```

同一个 compose 服务可以这样写：

```yaml
services:
  ht:
    image: ghcr.io/jiangood/ht:latest
    restart: unless-stopped
    ports:
      - "7777:7777"
    volumes:
      - ./data:/app
    stop_grace_period: 40s
```

注意：

- 一个镜像同时用于入口、中继和叶子：配置了 `[upstream]` 就向上游注册，没有就只做入口；不需要换镜像或换启动命令。
- Docker 用 `SIGTERM` 停止容器，`ht` 会像处理 `SIGINT` 一样开始优雅关闭；默认最多 drain 30 秒，所以用 `--stop-timeout 40`，或 compose 的 `stop_grace_period: 40s`。否则 Docker 默认 10 秒后会 `SIGKILL`，在途访客会被截断。
- 端口映射必须与 `ht.toml` 的 `server_port` 一致；管理页/API 也在这个端口上，无需额外映射。
- 需要 Web 管理页/API 时，挂载的 `/app` 必须可写（API 会用同目录临时文件原子回写 `ht.toml`）；请挂载整个目录（如 `./data:/app`），不要只挂载 `ht.toml` 单文件——单文件 bind mount 无法被 rename 覆盖，保存会失败。只想只读运行就用 `HT_PROXIES` 让代理列表只读。
- `local_addr` 在容器内解析：`127.0.0.1:80` 指容器本身，不是宿主机。转发宿主机服务请用 `host.docker.internal:<端口>`，转发同一 compose 中的服务用服务名。
- 容器内 `bind_host` 保持默认 `0.0.0.0`，否则 Docker 端口映射进不来。
- 镜像是基于 `scratch` 的静态镜像，没有 shell；用 `docker logs ht` 查看日志，不要把 `docker exec ... sh` 当作运维手段。静态 musl 二进制已内置 `wss` 所需的 WebPKI 根证书，不依赖基础镜像的 CA 包。
- 镜像没有内置 `HEALTHCHECK`（`scratch` 里没有 shell/curl）：在 compose 或 Kubernetes 里对 `server_port` 做 TCP 检查即可；需要 HTTP 健康端点的话，后续再单独加一个只读 `/healthz`。
- 镜像默认以 root 运行，方便首次生成和回写 `/app/ht.toml`；需要非 root 时用 `--user 65532:65532`，并保证挂载的配置目录对该 UID 可写。

## 从旧版本迁移（历史说明）

旧版本提供 `hts`、`htc`、`htn` 三个程序和三份配置。现在统一为 `ht` + `ht.toml`：

1. 旧 `hts.toml` 的 `server_port`、`bind_host`、`server_host`、`token`、`[[proxies]]` 原样保留；删除程序名即可作为独立入口使用。
2. 旧 `htc.toml` 的 `[[tunnels]]` 改名为 `[[proxies]]`（或保留旧名作为别名）；把顶层 `server` 移到 `[upstream].server`，补上 `[upstream].token`；`server_port` 建议设为 `0`，`bind_host = "127.0.0.1"`。管理页/API 现在与 `server_port` 共用端口，删除旧的 `web_port` / `web_host` 即可，访问地址为 `http://<server_host>:<server_port>/`。
3. 旧 `htn.toml` 几乎就是新的 `ht.toml`：把 `[upstream]` 的 `mirror`/`claim` 规则保持不变即可。
4. 环境变量从 `HTC_*` 迁移到 `HT_*`；旧名称仍可作为回退。
5. 旧的三类镜像 `hts`/`htc`/`htn` 已合并为 `ht`；原来三个容器改用一个镜像，用各自的 `ht.toml` 区分入口、中继和叶子。

## 许可证

[Apache-2.0](./LICENSE)
