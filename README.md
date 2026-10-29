# http-tunnel

[![Release](https://img.shields.io/github/v/release/jiangood/http-tunnel)](https://github.com/jiangood/http-tunnel/releases)
[![Build](https://img.shields.io/github/actions/workflow/status/jiangood/http-tunnel/rust-checks.yml?branch=main)](https://github.com/jiangood/http-tunnel/actions)
[![License](https://img.shields.io/github/license/jiangood/http-tunnel)](./LICENSE)

**http-tunnel** 是一个基于 Rust 的 HTTP 反向隧道工具：按 HTTP `Host` 把公网请求转发到内网服务，进程之间只使用 WebSocket（`ws://` / `wss://`）。

`ht` 是单一通用程序，角色只由 `ht.toml` 是否包含 `[upstream]` 决定：没有就是独立入口；有就是 relay/leaf，在服务本地 `[[proxies]]` 的同时把路由镜像给上一跳。

## 功能

- 按 `Host` 路由，支持多域名和 `*.example.com` 泛域名
- 隧道只使用 WebSocket，推荐 `wss://`；`ws://` 仅用于可信网络
- `[[proxies]]` 支持 TCP 与 Unix Socket，可选用 `X-HTTP-TUNNEL-TOKEN` 做入口校验
- 一条 v4 会话承载所有域名和访客流，多跳时一个进程一跳
- 管理页和 API 可增删本地服务，立即生效并写回 `ht.toml`

## 快速开始

从 [Releases](https://github.com/jiangood/http-tunnel/releases) 下载 `ht`，放进空目录运行一次：

```sh
./ht
```

没有 `ht.toml` 时，程序会写出带注释的模板并以退出码 0 退出。按下面的场景改好配置，再运行 `./ht`。命令行只有 `ht [-c/--config ht.toml] [-b/--background]`，`-b` 把输出写入当前目录的 `ht.log`。

## 拓扑与配置

| 场景 | 节点数 | 适用情况 | 关键区别 |
| --- | --- | --- | --- |
| 单节点 | 1 | 有公网服务器，服务也在这台机器或同一内网 | 不需要 `[upstream]` |
| 两个节点 | 2 | 公网入口 + 家庭/公司内网机器 | 客户端用 `[upstream]` 主动拨出 |
| 三个节点 | 3 | 内网机器不能直接访问公网入口，需要跳板 | 中间多一层中继 |

下面用 `app.example.com` 表示业务域名，`control.example.com` 表示入口的保留域名。Token 可用 `openssl rand -hex 16` 生成，示例里的中文 Token 必须替换。三种拓扑只是最常见的组合，入口和中继都可以同时代理本地服务。

### 单节点：入口 + 本地代理

```text
访客 ──> ht（control.example.com）──> 127.0.0.1:3000
```

`ht.toml`：

```toml
name = "edge"
listen_port = 7777
control_host = "control.example.com"
token = "把 openssl rand -hex 16 的输出填在这里"

[[proxies]]
name = "app"
domain = "app.example.com"
local_addr = "127.0.0.1:3000"
```

把 `app.example.com` 的 A 记录指向服务器后访问 `http://app.example.com:7777`；未配置 DNS 时可用 `curl -H "Host: app.example.com" http://127.0.0.1:7777/` 验证。管理页在 `http://control.example.com:7777/`，用户名 `ht`，密码是 `token`。

要点：

- `control_host` 是保留 Host，不能和任何 `[[proxies]].domain` 相同，也不支持泛域名。
- 公网部署应在入口前加 TLS 终止层，把 443 反代到 `127.0.0.1:7777`，见[传输模式与 TLS](#传输模式与-tls)。

### 两个节点：服务端 + 客户端

```text
访客 ──> 服务端 ht（control.example.com:443）
                ^ 一条 wss 长连接，客户端主动拨出
                │
         客户端 ht（app.example.com -> 127.0.0.1:3000）
```

服务端只做入口，客户端主动连接服务端，内网不需要开放任何入站端口。

服务端 `ht.toml`：

```toml
name = "ingress"
listen_port = 7777
control_host = "control.example.com"
token = "把 openssl rand -hex 16 的输出填在这里（客户端用同一个）"
```

客户端 `ht.toml`：

```toml
name = "home"
listen_port = 0

[[proxies]]
name = "app"
domain = "app.example.com"
local_addr = "127.0.0.1:3000"

[upstream]
url = "wss://control.example.com:443"
token = "与服务端 token 相同"
```

先启动服务端，再启动客户端，把 `app.example.com` 的 A 记录指向服务端后访问 `https://app.example.com`。还没有 TLS 时，可先把客户端的 `[upstream].url` 改成 `ws://control.example.com:7777`，用 `http://app.example.com:7777` 验证链路。

要点：

- 服务端的 `token` 与客户端的 `[upstream].token` 必须一致；客户端可省略顶层 `token`（复用上游 Token）和 `control_host`（默认 `127.0.0.1`）。
- 客户端 `listen_port = 0` 的实际端口见启动日志；服务端不需要写 `[[proxies]]`，它按 `Host` 把请求转给已连接客户端声明的域名。
- 没有域名时，把服务端的 `control_host` 和客户端的 `[upstream].url` 改成同一个 IP，两边主机名必须一致；同时在服务端放行 `7777`（以及 TLS 的 `443`）。

### 三个节点：服务端 + 中继 + 客户端

```text
访客 ──> 服务端 ht（control.example.com）
                ^ wss
                │
          中继 A ht（a.lan:7777）
                ^ ws（内网）或 wss（公网）
                │
          客户端 ht（app.example.com -> 127.0.0.1:3000）
```

适用于“客户端能访问 A，A 能访问公网，但客户端不能直接访问公网入口”的网络。服务端配置与上一节相同，中继 A 多一份自己的 `[upstream]`：

```toml
name = "relay-a"
listen_port = 7777
control_host = "a.lan"
token = "中继密钥（客户端使用）"

[upstream]
url = "wss://control.example.com:443"
token = "入口密钥（与服务端 token 相同）"
```

客户端只把 `[upstream]` 换成中继 A，其余与上一节相同：

```toml
[upstream]
url = "ws://a.lan:7777"
token = "中继密钥（与 A 的 token 相同）"
```

启动顺序：服务端 -> 中继 A -> 客户端。

要点：

- 每一跳使用不同的 Token：入口只信任 A，A 只信任客户端。
- 上游节点的 `control_host` 必须等于下游 `[upstream].url` 里的主机名，否则 v4 握手会被拒绝。
- A 的 `listen_port` 需允许客户端访问；按同样方式还可以继续加节点。

### 验证与常见问题

```sh
# 把地址改成入口所在机器
curl -i -H "Host: app.example.com" http://127.0.0.1:7777/
```

| 现象 | 原因 |
| --- | --- |
| `404 No tunnel for this host` | 客户端还没连上，或域名不在任何 `[[proxies]]` 中 |
| v4 会话返回 `401` | 两端 Token 不一致 |
| WebSocket 握手失败 | `control_host` 与 `[upstream].url` 的主机名不一致，或 TLS 终止层没有转发 `Upgrade` / `Connection` 头 |
| 能连上但访问超时 | `local_addr` 从客户端不可达；容器内的 `127.0.0.1` 指容器本身 |

## 配置参考

| 键 | 说明 |
| --- | --- |
| `name` | 进程名称，默认 `ht` |
| `listen_port` | 唯一的 HTTP 入口端口，访客、下游 v4 会话和管理页/API 共用；`0` 为临时端口，仅在有 `[upstream]` 时允许 |
| `control_host` | 留给 v4 会话入口和管理页/API 的精确 Host，不能是泛域名，也不能与任何 `[[proxies]]` 域名重叠；无 `[upstream]` 时必填，有则默认 `127.0.0.1` |
| `token` | 下游对等节点使用的监听 Token；有 `[upstream]` 且省略时复用 `[upstream].token` |
| `retry_interval` / `keepalive_interval` | 重连上一跳的间隔秒数 / WebSocket ping 间隔秒数（`0` 关闭保活） |
| `max_pending_handshakes` / `max_sessions` | 待握手升级数上限 / v4 会话上限 |
| `[[proxies]]` | 本进程直接服务的本地服务，字段见[转发配置](#转发配置) |
| `[upstream]` | 上一跳：`url` 为不带路径的 `ws://` / `wss://` 地址，`token` 为共享密钥；省略即独立入口 |

监听器始终绑定 `0.0.0.0`。`listen_port = 0` 的实际端口见启动日志，管理页也使用它。

### 上游通告

有 `[upstream]` 时，本进程只建立**一条** v4 会话，把本地 `[[proxies]]` 和下游会话声明的域名实时镜像到上一跳；新增或删除路由会立即通告或撤销，没有额外开关。建议每一跳使用不同的 Token，重复时启动会告警。

- 旧版本的 `mirror`、`claim` 和 `max_mirrored_routes` 已移除；配置里再出现这些键会启动失败，并提示它们已移除。
- 指向自身监听地址的 `[upstream]` 会被拒绝；DNS 解析回本机的检查在启动时进行。

### 环境变量

每个配置项都有对应的 `HT_*` 环境变量，环境变量优先于 `ht.toml`，只影响运行时配置：

| 环境变量 | 对应 `ht.toml` | 说明 |
| --- | --- | --- |
| `HT_NAME` | `name` | 进程名称 |
| `HT_UPSTREAM_URL` | `[upstream].url` | `ws://` 或 `wss://` 地址；设置后自动启用上游 |
| `HT_TOKEN` | `token` | 监听端口的共享 Token |
| `HT_UPSTREAM_TOKEN` | `[upstream].token` | 上一跳的 Token（可选） |
| `HT_PROXIES` | `[[proxies]]` | TOML 内联数组 |
| `HT_LISTEN_PORT` | `listen_port` | 监听端口，`0` 表示临时端口 |
| `HT_CONTROL_HOST` | `control_host` | v4 会话入口与管理页的保留 Host |
| `HT_RETRY_INTERVAL` | `retry_interval` | 重连上一跳的间隔秒数 |
| `HT_KEEPALIVE_INTERVAL` | `keepalive_interval` | WebSocket ping 间隔秒数 |

```sh
HT_UPSTREAM_URL=wss://control.example.com:443 HT_TOKEN=<上游 token> \
  HT_PROXIES='[{name="nas",domain="nas.example.com",local_addr="127.0.0.1:80"}]' ./ht
```

- 未设置或值为空字符串的变量会被忽略；环境变量不会写回 `ht.toml`。
- `HT_PROXIES` 整体替换文件里的 `[[proxies]]`，不合并；`HT_PROXIES='[]'` 表示不启用本地服务。
- 本地服务来自 `HT_PROXIES` 时，网页和 `PUT`/`DELETE /api/proxies/*` 都会被拒绝（`409`），修改环境变量并重启后才会生效。
- `ht.toml` 不存在时，只要设置了任一 `HT_*`，进程就用默认值加环境变量启动，**不会**生成或写入 `ht.toml`。

### 从 1.x 迁移

2.0 统一了监听端与控制面的命名。旧键会在启动时被拒绝，旧环境变量也会明确报错：

| 1.x | 2.0 |
| --- | --- |
| `server_port` | `listen_port` |
| `server_host` | `control_host` |
| `bind_host` | 已移除；监听器始终绑定 `0.0.0.0` |
| `[upstream].server` | `[upstream].url` |
| `HT_SERVER` | `HT_UPSTREAM_URL` |
| `HT_SERVER_PORT` | `HT_LISTEN_PORT` |
| `HT_SERVER_HOST` | `HT_CONTROL_HOST` |
| `HT_BIND_HOST` | 已移除 |

`HT_TOKEN`、`HT_NAME`、`HT_PROXIES`、`HT_RETRY_INTERVAL`、`HT_KEEPALIVE_INTERVAL` 和 `[upstream].token` 不变。

## 传输模式与 TLS

`[upstream].url` 的 scheme 决定是否使用 TLS：

| 示例 | 含义 |
| --- | --- |
| `wss://control.example.com`（推荐） | TLS WebSocket，默认端口 443，公网部署必须使用 |
| `ws://127.0.0.1:7777` | 明文 WebSocket，默认端口 80，仅适合可信网络或本地测试 |

`wss` 依赖可信的 TLS 终止层（Nginx、Caddy、云负载均衡或安全网关）提供服务端证书，`ht` 自身不申请证书。Nginx 示例（`proxy_read_timeout` 必须大于 `keepalive_interval`）：

```nginx
server {
    listen 443 ssl;
    # 必须同时包含 v4 会话入口和各节点的访客域名
    server_name control.example.com app.example.com;

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

`server_name` 和证书必须覆盖 `control_host` 与该入口服务的所有访客域名。**不要在入口上开启“HTTP 强制跳转 HTTPS”**：WebSocket 客户端不会跟随 301/308 重定向。

> **不要把一个到 `ht` 的 keep-alive 连接复用于多个域名。**
> `ht` 只读取一条连接上**第一个**请求的 `Host` 来选后端，之后整条连接原样透传（这样才能透明支持 WebSocket）。
> 如果 Nginx 的 `upstream { keepalive N; }`、Caddy 的连接池或云负载均衡把不同 `Host` 的请求复用到同一条到 `ht` 的连接，后续请求会被送到第一个域名对应的本地服务，表现为**并发升高后偶发 404，且响应来自后端、不带 `x-http-tunnel-request-id`**；负载降下来、连接池回收后又恢复正常。
>
> 处理方式（任选其一）：
> - 每个域名使用独立的 `upstream` 块（不要共用同一个 `upstream` 的 keepalive 池）；
> - 关闭到 `ht` 的 upstream keep-alive（去掉 `keepalive N`，或用 `proxy_http_version 1.0` + `proxy_set_header Connection close`）；
> - 让前端负载均衡为每个域名使用独立的 target group / 后端连接池。

## 转发配置

所有本地服务都使用 `[[proxies]]`：

| 字段 | 说明 |
| --- | --- |
| `name` | 规则名称，进程内唯一 |
| `domain` | 要匹配的域名，支持如 `*.example.com` 的泛域名 |
| `local_addr` | 本地目标地址，可使用 TCP 地址或 Unix Socket |
| `auth_header` | 可选；设置后访客必须携带 `X-HTTP-TUNNEL-TOKEN: <密钥>`，否则返回 `401` |

`local_addr` 是原始 TCP 地址（`127.0.0.1:3000`）或 Unix Socket（`unix:///var/run/docker.sock`），不是 URL：写成 `http://127.0.0.1:3000` 会在启动或保存规则时报错。

`auth_header` 校验通过后，`X-HTTP-TUNNEL-TOKEN` 会在转发给后端之前被移除，后端看不到密钥。使用自定义头而不是 `Authorization`，不会和后端自己的登录鉴权冲突。

## 多跳细节

- 数据面是递归的：上游会话把访客转发到本进程的 `127.0.0.1:<实际 listen_port>`，本进程再按 `Host` 路由一次；`Host` 和报文不改写，因此 WebSocket 升级可以穿过整条链。
- 关闭时先停止上游会话（撤销路由），再 drain 进行中的访客（最多 30 秒），最后关闭下游会话。

更多细节见 [docs/relay.md](./docs/relay.md)。

## 资源限制与保活

```toml
# 已认证但尚未完成 v4 握手的升级数
max_pending_handshakes = 256
# v4 会话上限（每个下游进程 1 条）
max_sessions = 256
# WebSocket ping 间隔（秒），0 关闭
keepalive_interval = 15
```

- 未认证的升级在 `Authorization: Bearer` 校验失败时直接返回 `401`，不会占用上述额度。
- 超过 `max_pending_handshakes` 时返回 `503`；升级握手最长等待 10 秒，v4 会话握手最长等待 5 秒。
- 单个 WebSocket 帧/消息最大 1 MiB；流数据帧为 64 KiB，每个方向有 256 KiB 的初始窗口，接收方消费后发送窗口更新。
- 每个 WebSocket 连接每 `keepalive_interval` 秒发送一次 ping；连续 3 个间隔没有收到任何帧就关闭连接。

## Web 管理

管理页面与 API 始终开启，和访客、v4 会话共用 `listen_port`，只在保留的 `control_host` 上可达。浏览器打开 `http://<control_host>:<listen_port>/`，HTTP Basic 用户名填 `ht`，密码填 `token`；页面使用原生 HTML 表单，不包含 CSS 或 JavaScript。`control_host` 不是本机可解析的地址时，可用 `curl -H "Host: <control_host>" http://127.0.0.1:<listen_port>/` 访问；`listen_port = 0` 时管理页也用临时端口，以启动日志为准。

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/api/status` | 查看状态和本地服务数 |
| GET | `/api/proxies` | 列出本地 `[[proxies]]` |
| GET | `/api/routes` | 查看当前生效的路由表（本地服务 + 下游 v4 路由及其所属节点） |
| PUT | `/api/proxies/{name}` | 新增或更新本地服务 |
| DELETE | `/api/proxies/{name}` | 删除本地服务 |

修改会立即更新运行中的代理列表和路由 watch，并写回 `ht.toml`。详见 [docs/management-api.md](./docs/management-api.md)。

## 错误响应与排查

`ht` 自己生成的错误响应都是纯文本，并带有同一个请求 ID：

- 响应头 `x-http-tunnel-request-id: 0123456789abcdef`
- 响应体最后一行 `Request ID: 0123456789abcdef`
- 日志里同名字段 `request_id=0123456789abcdef`

因此浏览器里看到错误时，可以直接用响应体或 `curl -i` 里的 ID 搜索日志：

```sh
curl -i -H "Host: app.example.com" http://127.0.0.1:7777/
docker logs ht 2>&1 | grep 0123456789abcdef
```

| 状态 | 含义 |
| --- | --- |
| `400` / `408` / `431` | 访客请求头无法解析、读取超时或过大 |
| `401` | 本地服务配置了 `auth_header`，但访客缺少或带错 `X-HTTP-TUNNEL-TOKEN` |
| `404` | 没有任何路由匹配该 `Host`，或 `control_host` 上的路径不存在 |
| `502` | 匹配到了本地服务或 v4 隧道，但连接/打开流失败 |
| `503` | 隧道暂不可用（例如流或会话达到上限） |
| `504` | 隧道在超时前没有返回任何响应 |

管理 API 的错误是 JSON：`{"error":"...","request_id":"..."}`；未知的 `/api/*` 路径还会在 `error` 中写明方法和路径。**`ht` 不会返回 `{"message":"page not found"}`**：如果看到这种响应，且响应头里没有 `x-http-tunnel-request-id`，说明它来自前置反向代理、CDN、托管平台或本地服务，而不是 `ht`。

如果这种响应只在**并发升高时**出现、过一段时间又消失，优先检查前置层是否把不同 `Host` 的请求复用到同一条到 `ht` 的 keep-alive 连接（见上一节的注意事项）。

`404` 的日志会带上**当时的路由表内容**，可以直接看出“表里有什么、缺什么”，不用再去翻变更历史：

```
INFO http_tunnel::http: No route matches the host; answered 404
     request_id=a472f3bb090d3ee3 host=safeline.example.com
     local_proxies=1 v4_routes=1 v4_sessions=1/256
     known_hosts_total=2
     known_hosts=["app.example.com (local:web)", "svc.example.com (v4:3)"]
```

- `known_hosts_total=0` 且 `v4_sessions=0/…`：没有任何下游节点在线。
- `known_hosts` 里没有该域名：`ht` 上根本没有这条路由，检查 `[[proxies]]`、`HT_PROXIES` 和前置代理的转发目标。
- `v4_sessions` 有值但没有该域名：节点在线，但它广告的是别的域名（配置或冲突）。
- 同一行还出现 `withdrawn_prefix`：这条路由刚刚被撤销，通常是所属隧道在重连。

默认日志级别是 `info`，只记录路由失败（`404`/`502`/`504` 等）和会话事件。要看每个访客的路由决策（匹配到哪个服务、转发到哪个地址、v4 流的开启与结束），用 `RUST_LOG` 提高级别，无需重新编译：

```sh
RUST_LOG=debug ./ht -c ht.toml          # 全部 debug
RUST_LOG=http_tunnel=debug ./ht -c ht.toml  # 只开 ht 自己的 debug
docker run ... -e RUST_LOG=debug ghcr.io/jiangood/ht:latest
```

多跳时，同一个 `request_id` 会写进 v4 `OpenStream` 的 `trace_id`，所以中继和叶子节点的日志可以用同一个 ID 关联（每一跳的嵌入式服务会重新生成 ID，按 Host 和时间即可对上）。

## Docker

只发布一个通用镜像，入口、中继和叶子都用它：配置了 `[upstream]` 就向上游注册，没有就只做入口。

```sh
docker run -d --name ht --restart unless-stopped \
  --stop-timeout 40 \
  -p 7777:7777 \
  -v "/opt/ht:/app/" \
  ghcr.io/jiangood/ht:latest
```

容器固定读取 `/app/ht.toml`，也可以用 `HT_*` 环境变量配置（见[环境变量](#环境变量)）。compose 部署时把 `stop_grace_period` 设为 `40s` 并挂载 `./data:/app`。

注意：

- Docker 用 `SIGTERM` 停止容器，`ht` 会优雅关闭并最多 drain 30 秒；Docker 默认只等 10 秒就 `SIGKILL`，所以要显式设置 `--stop-timeout 40`（或 compose 的 `stop_grace_period: 40s`），否则在途访客会被截断。
- 端口映射必须与 `listen_port` 一致，管理页/API 也在这个端口上，无需额外映射。
- 保存管理页修改需要 `/app` 可写（会原子回写 `ht.toml`），请挂载整个目录；只想只读运行就用 `HT_PROXIES` 让代理列表只读。
- `local_addr` 在容器内解析：`127.0.0.1` 指容器本身，转发宿主机服务用 `host.docker.internal:<端口>`，转发同一 compose 中的服务用服务名。
- 镜像是基于 `scratch` 的静态镜像，没有 shell，用 `docker logs ht` 查看日志；默认以 root 运行，可用 `--user 65532:65532` 并保证挂载目录对该 UID 可写。
- 镜像没有内置 `HEALTHCHECK`，对 `listen_port` 做 TCP 检查即可。

## 许可证

[Apache-2.0](./LICENSE)
