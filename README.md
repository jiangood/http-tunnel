# http-tunnel

[![Release](https://img.shields.io/github/v/release/jiangood/http-tunnel)](https://github.com/jiangood/http-tunnel/releases)
[![Build](https://img.shields.io/github/actions/workflow/status/jiangood/http-tunnel/rust-checks.yml?branch=main)](https://github.com/jiangood/http-tunnel/actions)
[![License](https://img.shields.io/github/license/jiangood/http-tunnel)](./LICENSE)

**http-tunnel** 是一个基于 Rust 的 HTTP 反向隧道工具：按 HTTP `Host` 把公网请求转发到内网服务，进程之间只使用 WebSocket（`ws://` / `wss://`）。

`ht` 是单一通用程序，角色由 `ht.toml` 里的两个表决定：`[server]` 决定是否监听，`[upstream]` 决定是否向上游拨出。只有 `[server]` 是独立入口，只有 `[upstream]` 是纯客户端/叶子，两者都有就是中继。

## 功能

- 按 `Host` 路由，支持多域名和 `*.example.com` 泛域名
- 隧道只使用 WebSocket，推荐 `wss://`；`ws://` 仅用于可信网络
- `[[proxies]]` 支持 TCP 与 Unix Socket，可选用 `X-HTTP-TUNNEL-TOKEN` 做入口校验
- 一条 v4 会话承载所有域名和访客流，多跳时一个进程一跳
- 管理页和 API 可增删改本地服务，立即生效并写回 `ht.toml`；也能管理已连接下游节点的 `[[proxies]]` 并断开其会话
- 管理页显示活跃访客和本次运行的累计连接，以及热加载错误；可筛选路由、测试本地服务、导出配置；保存配置时保留原有注释
- `[server].bind` 可把监听限制在 `127.0.0.1` 等指定地址，便于同机反向代理；不写 `[server]` 则完全不监听，只作为客户端拨出

## 快速开始

下面用一台服务器和一个客户端跑通完整链路：服务器在公网监听，客户端在内网主动拨出，把内网机器的 `http://127.0.0.1:3000` 暴露成 `app.example.com`。

```text
访客 ──> 服务器 ht（control.example.com:7777）
                ^ 客户端主动建立的 ws / wss 长连接
                │
         客户端 ht（app.example.com -> 127.0.0.1:3000）
```

`control.example.com` 是服务器的保留域名，只用于管理页和下游会话，不承载业务流量。Token 可用 `openssl rand -hex 16` 生成，两端必须一致；示例里的中文占位符都要替换。

### 1. 准备

从 [Releases](https://github.com/jiangood/http-tunnel/releases) 下载 `ht`，在服务器和内网机器上各放一份：

直接运行 `./ht`，程序会写出带注释的默认配置并直接以服务端模式启动（默认监听 `0.0.0.0:7777`，`[server].token` 随机生成，`[upstream]` 默认注释掉），不会退出。按下面的内容改好配置后保存，运行中的 `ht` 会自动热加载。

### 2. 服务端

`server/ht.toml`：

```toml
name = "ingress"

[server]
port = 7777
bind = "0.0.0.0"
domain = "control.example.com"
token = "把 openssl rand -hex 16 的输出填在这里（客户端用同一个）"
```

它按 `Host` 把访客请求转给已连接客户端声明的域名。`bind = "0.0.0.0"` 是为了让客户端能从公网连上

### 3. 客户端

`client/ht.toml`：

```toml
name = "home"

[[proxies]]
name = "app"
domain = "app.example.com"
local_addr = "127.0.0.1:3000"

[upstream]
url = "ws://control.example.com:7777"
token = "与服务端 token 相同"
```

- `local_addr` 指向内网机器上已经在监听的服务；示例假设 `127.0.0.1:3000` 可访问。
- 不写 `[server]`，客户端就完全不监听端口，只向上游拨出，内网不需要开放任何入站端口。
- `[upstream].url` 里的主机名必须与服务器的 `[server].domain` 完全一致，否则 v4 握手会被拒绝。
- 还没有 DNS 时，把两边的 `control.example.com` 都换成服务器的 IP，两处主机名保持相同即可。
- 这里先用 `ws://` 验证链路；公网正式部署时换成 `wss://control.example.com`（默认 443），并在服务器前加 TLS 终止层，见[传输模式与 TLS](#传输模式与-tls)。

### 4. 启动

先启动服务端，再启动客户端：

```sh
# 公网服务器
cd server && ./ht

# 内网机器（另一个终端或另一台机器）
cd client && ./ht
```

需要常驻时加 `-b`，输出写入当前目录的 `ht.log`；`-c` 可以指定别的配置文件。命令行只有 `ht [-c/--config ht.toml] [-b/--background]`。

### 5. 验证

```sh
# 在服务器上，用 Host 头模拟 DNS 解析
curl -i -H "Host: app.example.com" http://127.0.0.1:7777/

# 或把 app.example.com 的 A 记录指向服务器后
curl -i http://app.example.com:7777/
```

返回客户端 `127.0.0.1:3000` 的内容，就说明整条链路已经打通。管理页在 `http://control.example.com:7777/`，用户名 `ht`，密码是服务端的 token；域名还没解析时用 `curl -H "Host: control.example.com" http://127.0.0.1:7777/`。

### 6. 下一步

- 只有一台机器（入口和本地服务同机）：[单节点：入口 + 本地代理](#单节点入口--本地代理)
- 客户端不能直接访问公网入口：[三个节点：服务端 + 中继 + 客户端](#三个节点服务端--中继--客户端)
- 公网部署：补上 [TLS](#传输模式与-tls)，并给敏感服务的 `[[proxies]]` 加 `auth_header`
- 连不上或返回错误：[验证与常见问题](#验证与常见问题)、[错误响应与排查](#错误响应与排查)

## 拓扑与配置

| 场景 | 节点数 | 适用情况 | 关键区别 |
| --- | --- | --- | --- |
| 单节点 | 1 | 有公网服务器，服务也在这台机器或同一内网 | 只有 `[server]` |
| 两个节点 | 2 | 公网入口 + 家庭/公司内网机器 | 服务端有 `[server]`，客户端只有 `[upstream]` |
| 三个节点 | 3 | 内网机器不能直接访问公网入口，需要跳板 | 中间节点同时有 `[server]` 和 `[upstream]` |

下面用 `app.example.com` 表示业务域名，`control.example.com` 表示入口的保留域名。Token 可用 `openssl rand -hex 16` 生成，示例里的中文 Token 必须替换。三种拓扑只是最常见的组合，入口和中继都可以同时代理本地服务。

### 单节点：入口 + 本地代理

```text
访客 ──> ht（control.example.com）──> 127.0.0.1:3000
```

`ht.toml`：

```toml
name = "edge"

[server]
port = 7777
bind = "0.0.0.0"
domain = "control.example.com"
token = "把 openssl rand -hex 16 的输出填在这里"

[[proxies]]
name = "app"
domain = "app.example.com"
local_addr = "127.0.0.1:3000"
```

把 `app.example.com` 的 A 记录指向服务器后访问 `http://app.example.com:7777`；未配置 DNS 时可用 `curl -H "Host: app.example.com" http://127.0.0.1:7777/` 验证。管理页在 `http://control.example.com:7777/`，用户名 `ht`，密码是 `[server].token`。

要点：

- `[server].domain` 是保留 Host，不能和任何 `[[proxies]].domain` 相同，也不支持泛域名。
- `[server].bind` 在生成的配置里是 `0.0.0.0`，默认监听所有网卡，方便 Docker 发布端口和公网直连；只允许本机 Nginx/Caddy 反代时改成 `127.0.0.1`。公网部署务必设置强 Token 并用防火墙限制来源。
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

[server]
port = 7777
bind = "0.0.0.0"
domain = "control.example.com"
token = "把 openssl rand -hex 16 的输出填在这里（客户端用同一个）"
```

客户端 `ht.toml`：

```toml
name = "home"

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

- 服务端的 `[server].token` 与客户端的 `[upstream].token` 必须一致；客户端不写 `[server]`，也不写 `[server].token`（需要监听时才设置）。
- 客户端完全不监听端口；服务端不需要写 `[[proxies]]`，它按 `Host` 把请求转给已连接客户端声明的域名。
- 没有域名时，把服务端的 `[server].domain` 和客户端的 `[upstream].url` 改成同一个 IP，两边主机名必须一致；同时在服务端放行 `7777`（以及 TLS 的 `443`）。

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

[server]
port = 7777
bind = "0.0.0.0"
domain = "a.lan"
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
- 上游节点的 `[server].domain` 必须等于下游 `[upstream].url` 里的主机名，否则 v4 握手会被拒绝。
- A 的 `[server].port` / `bind` 需允许客户端访问；按同样方式还可以继续加节点。

### 验证与常见问题

```sh
# 把地址改成入口所在机器
curl -i -H "Host: app.example.com" http://127.0.0.1:7777/
```

| 现象 | 原因 |
| --- | --- |
| `404 No tunnel for this host` | 客户端还没连上，或域名不在任何 `[[proxies]]` 中 |
| v4 会话返回 `401` | 两端 Token 不一致 |
| WebSocket 握手失败 | `[server].domain` 与 `[upstream].url` 的主机名不一致，或 TLS 终止层没有转发 `Upgrade` / `Connection` 头 |
| 能连上但访问超时 | `local_addr` 从客户端不可达；容器内的 `127.0.0.1` 指容器本身 |

## 配置参考

下表列出 `ht.toml` 的全部配置项，`[[proxies]]` 可重复出现：

| 键 | 必填 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `name` | 否 | `ht` | 进程名称，用于日志和 v4 节点标识；顶层唯一的标量配置 |
| `[server]` | 条件 | 无 | 服务端模式开关；不写就完全不监听、只作为客户端向上游拨出。没有 `[upstream]` 时必须写 |
| `[server].port` | 否 | `7777` | 唯一的 HTTP 入口端口，访客、下游 v4 会话和管理页/API 共用；`0` 为临时端口，仅在有 `[upstream]` 时允许 |
| `[server].bind` | 否 | `127.0.0.1` | 监听地址，IPv4/IPv6 字面量；Docker、局域网中继或直接公网访问时设为 `0.0.0.0` |
| `[server].domain` | 否 | 无 | 入口 Host（原 `control_host`），启用管理页/API 和下游 v4 入口；省略时只监听访客路由。不能是泛域名，也不能与任何 `[[proxies]]` 域名重叠 |
| `[server].token` | 条件 | 无 | 下游对等节点使用的监听 Token；设置 `[server].domain` 时必须，省略时复用 `[upstream].token` |
| `[server].max_pending_handshakes` | 否 | `256` | 待完成 v4 握手的升级数上限 |
| `[server].max_sessions` | 否 | `256` | v4 会话上限（每个下游进程 1 条） |
| `[server].keepalive_interval` | 否 | `15` | 服务端 WebSocket ping 间隔（秒），`0` 关闭保活 |
| `[upstream]` | 条件 | 无 | 上一跳；省略即服务端入口。写入时必须提供 `url` 和 `token` |
| `[upstream].url` | 是（写 `[upstream]` 时） | 无 | 上一跳地址，不带路径的 `ws://` / `wss://` URL |
| `[upstream].token` | 是（写 `[upstream]` 时） | 无 | 与上一跳共享的 Token，必须与上一跳的 `[server].token` 一致 |
| `[upstream].allow_modify` | 否 | `false` | 是否允许上一跳通过 v4 管理通道修改本节点的 `[[proxies]]`；关闭时上级仍可列出服务但不能增删改。没有 `[server]` 时没有管理页，只能在配置文件里切换 |
| `[upstream].retry_interval` | 否 | `1` | 重连上一跳的间隔（秒） |
| `[upstream].keepalive_interval` | 否 | `15` | 上游 WebSocket ping 间隔（秒），`0` 关闭保活 |
| `[[proxies]]` | 否 | 无 | 本进程直接服务的本地服务，可写多条；字段见下面几行 |
| `[[proxies]].name` | 是 | 无 | 规则名称，进程内唯一 |
| `[[proxies]].domain` | 是 | 无 | 要匹配的域名，支持如 `*.example.com` 的泛域名 |
| `[[proxies]].local_addr` | 是 | 无 | 本地目标地址，可使用 TCP 地址或 Unix Socket |
| `[[proxies]].auth_header` | 否 | 无 | 设置后访客必须携带 `X-HTTP-TUNNEL-TOKEN: <密钥>`，否则返回 `401` |

表中“条件”表示按角色判断：`[server]` 与 `[upstream]` 至少要有一个；设置 `[server].domain` 时必须提供 `[server].token`（可直接复用 `[upstream].token`）。未列出的旧键会在启动时被拒绝。

`[server]` 生成的配置默认写入 `bind = "0.0.0.0"`，监听所有网卡；省略 `bind` 时仍按 `127.0.0.1`。`0.0.0.0` 会把端口暴露给所有可达网络：只允许本机 Nginx/Caddy 反代时改成 `127.0.0.1`；公网部署务必设置强 Token、用防火墙限制来源，并优先把管理面和业务流量放在 HTTPS 反代之后。`[server].port = 0` 的实际端口见启动日志，管理页也使用它。以上所有配置项和 `[[proxies]]` 都可以在管理页修改，保存后写回同一个 `ht.toml`。没有 `[server]` 时管理页不可达，只能通过配置文件或上游的 v4 管理通道维护 `[[proxies]]`。

### 配置热加载

进程运行时会以约 1 秒的间隔检查 `ht.toml`，内容变化后自动应用，不需要重启：

- `[[proxies]]` 和 `[upstream].allow_modify` 直接应用到运行中的进程：监听端口和上游会话都不断开，新增或删除服务会立即通告给上一跳。
- 其他配置项（`name`、`[server]`、`[upstream]` 的地址/Token、重连/保活间隔、并发上限）会重建进程内的监听逻辑和上游会话：`[server].port` 和 `[server].bind` 都不变时复用已绑定的监听 socket，端口不会暂时拒绝连接；端口变化时先绑定新端口，成功后再停止旧监听；只有 `bind` 在同一个端口上变化时，会先停止旧监听、释放地址再绑定新地址，失败则回滚到原地址继续服务。新增或删除整个 `[server]` 同样走这条路径。旧会话先撤销路由并等待在途访客（最多 30 秒），随后按新配置重新监听/连接并重新通告路由。`ht` 进程本身不退出。
- 正在编辑时写出的半成品或非法内容只会记录一条警告，继续使用上一份有效配置；保存完整后下一次检查会生效。最近一次热加载失败的原因和发生时间会显示在管理页顶部，也会出现在 `/api/status` 的 `last_reload_error`、`last_reload_error_seconds_ago` 和 `last_reload_success_seconds_ago` 中。
- 管理页/API 写回 `ht.toml` 时，只改 `[[proxies]]` 或 `[upstream].allow_modify` 会在线应用，不会触发重建；改动其他配置项则按上面的热加载流程重建。

> 端口、Token、上游地址这类配置必然会影响监听器或会话，应用时现有访客连接和下游会话会短暂断开；只改 `[[proxies]]` 时不会。

### 上游通告

有 `[upstream]` 时，本进程只建立**一条** v4 会话，把本地 `[[proxies]]` 和下游会话声明的域名实时镜像到上一跳；新增或删除路由会立即通告或撤销，没有额外开关。建议每一跳使用不同的 Token，重复时启动会告警。

- 旧版本的 `mirror`、`claim` 和 `max_mirrored_routes` 已移除；配置里再出现这些键会启动失败，并提示它们已移除。
- 指向自身监听地址的 `[upstream]` 会被拒绝；DNS 解析回本机的检查在启动时进行。

### 配置文件是唯一配置来源

3.0 起不再读取 `HT_*` 环境变量。`ht.toml` 是唯一的配置来源，管理页和 `/api/settings` 可以直接修改本节点的全部配置项并写回该文件：

- 首次启动时如果 `ht.toml` 不存在，`ht` 会生成带注释的默认配置并直接以服务端模式启动，不退出；`[server]` 默认启用并监听 `0.0.0.0:7777`，其中的 `token` 随机生成，`[upstream]` 默认注释掉。
- 容器部署也只需要挂载配置文件；不再支持“只有环境变量、没有配置文件”的启动方式。
- 旧部署中的 `HT_*` 变量会被忽略，请把需要的值迁移到 `ht.toml`。
- `[[proxies]]` 可以在管理页“管理服务”中增删改，其他配置项在“节点配置”中修改。

### 从 1.x / 2.x / 3.x 迁移

监听端与控制面的配置现在按角色分组，旧键会在启动时被拒绝并给出迁移提示：

| 旧配置 | 新配置 |
| --- | --- |
| `server_port` / `listen_port` | `[server].port` |
| `server_host` / `control_host` | `[server].domain` |
| `bind_host` | `[server].bind` |
| `allow_upstream_modify` | `[upstream].allow_modify` |
| `[upstream].server` | `[upstream].url` |
| 顶层 `token` | `[server].token` |
| 顶层 `retry_interval` | `[upstream].retry_interval` |
| 顶层 `keepalive_interval` | `[server].keepalive_interval` 和 `[upstream].keepalive_interval` |
| 顶层 `max_pending_handshakes` | `[server].max_pending_handshakes` |
| 顶层 `max_sessions` | `[server].max_sessions` |

现在顶层只保留 `name`（以及 `[server]`、`[upstream]`、`[[proxies]]` 三个表）；出现旧位置的其他键会在启动时被拒绝并给出迁移提示。

3.0 移除了全部 `HT_*` 环境变量（包括 `HT_PROXIES`、`HT_TOKEN`、`HT_UPSTREAM_URL` 等）。请把变量值写入 `ht.toml` 的对应键后再启动。

## 传输模式与 TLS

`[upstream].url` 的 scheme 决定是否使用 TLS：

| 示例 | 含义 |
| --- | --- |
| `wss://control.example.com`（推荐） | TLS WebSocket，默认端口 443，公网部署必须使用 |
| `ws://127.0.0.1:7777` | 明文 WebSocket，默认端口 80，仅适合可信网络或本地测试 |

`wss` 依赖可信的 TLS 终止层（Nginx、Caddy、云负载均衡或安全网关）提供服务端证书，`ht` 自身不申请证书。Nginx 示例（`proxy_read_timeout` 必须大于 `[server].keepalive_interval` / `[upstream].keepalive_interval`）：

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

`server_name` 和证书必须覆盖 `[server].domain` 与该入口服务的所有访客域名。**不要在入口上开启“HTTP 强制跳转 HTTPS”**：WebSocket 客户端不会跟随 301/308 重定向。如果 Nginx 和 `ht` 在同一台机器上，保持 `[server].bind = "127.0.0.1"` 即可，这样 `7777` 不会暴露到公网，也不需要额外防火墙规则。

> **不要把一个到 `ht` 的 keep-alive 连接复用于多个域名。**
> `ht` 只读取一条连接上**第一个**请求的 `Host` 来选后端，之后整条连接原样透传（这样才能透明支持 WebSocket）。
> 如果 Nginx 的 `upstream { keepalive N; }`、Caddy 的连接池或云负载均衡把不同 `Host` 的请求复用到同一条到 `ht` 的连接，后续请求会被送到第一个域名对应的本地服务，表现为**并发升高后偶发 404，且响应来自后端、不带 `x-http-tunnel-request-id`**；负载降下来、连接池回收后又恢复正常。
>
> 处理方式（任选其一）：
> - 每个域名使用独立的 `upstream` 块（不要共用同一个 `upstream` 的 keepalive 池）；
> - 关闭到 `ht` 的 upstream keep-alive（去掉 `keepalive N`，或用 `proxy_http_version 1.0` + `proxy_set_header Connection close`）；
> - 让前端负载均衡为每个域名使用独立的 target group / 后端连接池。

## 转发配置

所有本地服务都使用 `[[proxies]]`，各字段（`name`、`domain`、`local_addr`、`auth_header`）的必填项和默认值见[配置参考](#配置参考)。

`local_addr` 是原始 TCP 地址（`127.0.0.1:3000`）或 Unix Socket（`unix:///var/run/docker.sock`），不是 URL：写成 `http://127.0.0.1:3000` 会在启动或保存规则时报错。

`auth_header` 校验通过后，`X-HTTP-TUNNEL-TOKEN` 会在转发给后端之前被移除，后端看不到密钥。使用自定义头而不是 `Authorization`，不会和后端自己的登录鉴权冲突。

### 用隧道访问 Docker API

Docker API 是普通 HTTP，挂载 socket 后用一条 `[[proxies]]` 即可代理：

```toml
[[proxies]]
name = "docker"
domain = "docker.example.com"
local_addr = "unix:///var/run/docker.sock"
auth_header = "demo-visitor-token"
```

完整可运行的 compose、Dockerfile、Docker CLI 接入方式和 Windows 说明见 [examples/docker/client-docker-sock](./examples/docker/client-docker-sock/) 与 [examples/docker/README.md](./examples/docker/README.md)。Docker API 等价于宿主机 root：公网部署必须加 TLS 和 `auth_header`，优先只通过 VPN 或内网暴露。Windows 原生 `ht.exe` 不支持 Unix Socket，请让 ht 以 Linux 容器运行。

## 多跳细节

- 有 `[server]` 时数据面是递归的：上游会话把访客转发到本进程自己的监听地址（通配绑定用 `127.0.0.1:<实际 port>` 或 `[::1]`，具体 `bind` 则用该地址），本进程再按 `Host` 路由一次；`Host` 和报文不改写，因此 WebSocket 升级可以穿过整条链。
- 没有 `[server]` 时没有递归的一跳：v4 流按域名直接连接对应的 `[[proxies]].local_addr`，所以纯客户端不占用任何监听端口。
- 关闭时先停止上游会话（撤销路由），再 drain 进行中的访客（最多 30 秒），最后关闭下游会话。

更多细节见 [docs/relay.md](./docs/relay.md)。

## 资源限制与保活

```toml
[server]
# 已认证但尚未完成 v4 握手的升级数
max_pending_handshakes = 256
# v4 会话上限（每个下游进程 1 条）
max_sessions = 256
# 服务端 WebSocket ping 间隔（秒），0 关闭
keepalive_interval = 15

[upstream]
# 重连上一跳的间隔（秒）
retry_interval = 1
# 上游 WebSocket ping 间隔（秒），0 关闭
keepalive_interval = 15
```

- 未认证的升级在 `Authorization: Bearer` 校验失败时直接返回 `401`，不会占用上述额度。
- 超过 `[server].max_pending_handshakes` 时返回 `503`；升级握手最长等待 10 秒，v4 会话握手最长等待 5 秒。
- 单个 WebSocket 帧/消息最大 1 MiB；流数据帧为 64 KiB，每个方向有 256 KiB 的初始窗口，接收方消费后发送窗口更新。
- 每个 WebSocket 连接每 `keepalive_interval` 秒发送一次 ping；连续 3 个间隔没有收到任何帧就关闭连接。服务端和上游分别使用 `[server].keepalive_interval` 与 `[upstream].keepalive_interval`。

## Web 管理

配置了 `[server].domain` 时管理页面与 API 开启，和访客、v4 会话共用 `[server].port`，只在保留的 `[server].domain` 上可达。浏览器打开 `http://<server.domain>:<server.port>/`，HTTP Basic 用户名填 `ht`，密码填 `[server].token`；页面是一个内联 CSS/JS 的单页控制台，本身不做服务端渲染，所有数据都通过 `/api/*`（Bearer token）获取，可查看状态、活跃访客与本次运行的累计连接、热加载错误、编辑本节点全部配置、管理本地服务、筛选路由表、查看下游节点（含来源 IP）、接口列表和 `ht.toml` 配置项参考。节点卡片中本节点和下游节点的每一行都有“管理服务”，点开后可在弹窗内查看，本节点服务还可以一键“测试”`local_addr` 是否可达，或在允许时增删改该节点自己的 `[[proxies]]`；下游节点行还可以“断开”当前会话。顶部“导出配置”会下载包含注释和 Token 明文的 `ht.toml`；保存配置时会保留文件里的注释和未修改服务的说明。

**上级修改默认关闭。** 下游节点的 `[[proxies]]` 默认只能由本机管理页修改；上一跳仍可列出这些服务（包括 `auth_header` 明文），但写入会返回 `409`。下游节点可以在自己的管理页顶部“上级修改”中切换，切换会立即重连上游会话以重新上报能力位，期间该节点的路由会短暂撤销并重新通告。独立入口没有上游，页面显示“无上游”。

`[server].domain` 不是本机可解析的地址时，可用 `curl -H "Host: <server.domain>" http://127.0.0.1:<server.port>/` 访问；`[server].port = 0` 时管理页也用临时端口，以启动日志为准。没有 `[server]` 或没有 `domain` 时没有管理页。

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/api/status` | 查看状态、本地服务数、下游节点数、活跃访客与本次运行的累计连接、上级修改开关及是否可编辑、热加载错误、版本、配置文件、运行时长 |
| GET | `/api/settings` | 查看本节点的全部配置项（含 Token 明文，`[[proxies]]` 除外） |
| GET | `/api/settings/export` | 下载磁盘上的 `ht.toml`（含注释和 Token 明文），可作为备份 |
| GET | `/api/proxies` | 列出本地 `[[proxies]]`（含 `auth_header` 明文和来源）；带 `?node=` 或 `?session=` 时列出该下游节点的服务 |
| GET | `/api/routes` | 查看当前生效的路由表（本地服务在前，下游 v4 路由在后，均按域名排序） |
| GET | `/api/sessions` | 列出已连接的下游节点：会话号、节点、网关转发的来源 IP、TCP 对端、角色、连接时长、所属域名、是否可管理/可写与服务来源 |
| PUT | `/api/settings` | 替换本节点配置并写回 `ht.toml`；`?dry_run=true` 只检查是否需要重启，`?restart=true` 在需要时立即重建运行时。响应含 `restart_required`、`restart_triggered`、`saved` 和 `settings` |
| POST | `/api/restart` | 按磁盘上的 `ht.toml` 立即重建监听器和上游会话；`ht` 进程本身不退出 |
| POST | `/api/proxies/{name}/probe` | 从本节点拨号测试该服务的 `local_addr`，3 秒超时；返回 `ok`、`latency_ms` 和错误信息 |
| PUT | `/api/proxies/{name}` | 新增或替换本地服务，可设置/清除 `auth_header`；带 `?node=`/`?session=` 时写入下游节点 |
| PUT | `/api/settings/allow-upstream-modify` | 只开关本节点是否允许上级修改，请求体 `{"allowed":true}`；保存后立即重连上游会话 |
| DELETE | `/api/proxies/{name}` | 删除本地服务；带 `?node=`/`?session=` 时删除下游节点的服务 |
| DELETE | `/api/sessions/{id}` | 断开一个下游会话（节点会按自己的重连间隔重新连上） |

修改会立即更新运行中的代理列表和路由 watch，并写回 `ht.toml`；带下游参数时写回的是该节点的 `ht.toml`。`node` 与 `session` 可同时给出，必须指向同一会话；不给出时只操作本进程。上级修改开关只作用于本进程，不能带 `?node=`/`?session=`；没有 `[upstream]` 时返回 `409`。

在“节点配置”中保存需要重建运行时的配置时，页面会先弹框提示，确认后才保存并立即重启运行时；取消则不会写入文件。页面上的“重启运行时”按钮也可以随时按磁盘上的 `ht.toml` 强制重建监听器和上游会话。这里的“重启”不会退出 `ht` 进程，只重建监听器和上游会话，期间现有连接会短暂断开。

`GET /api/proxies` 会以明文返回 `auth_header`，管理响应带 `Cache-Control: no-store`；管理面仍必须通过 HTTPS/TLS 访问，且前置代理不要记录响应体。`PUT` 是整体替换：省略、`null` 或空字符串都会关闭该服务的访客认证，管理页会回填当前值。代理管理的帧结构已携带 `auth_header`，同一控制面的所有节点必须运行同一版本；入口会拒绝管理不支持该能力的旧下游并返回 `409`。详见 [docs/management-api.md](./docs/management-api.md)。

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
| `404` | 没有任何路由匹配该 `Host`，或 `[server].domain` 上的路径不存在 |
| `502` | 匹配到了隧道，但隧道背后的本地服务连不上 |
| `503` | 隧道会话无法打开流（正在关闭、流或会话达到上限、会话已断开） |
| `504` | 下游节点在超时前没有打开流，或隧道打开后没有返回任何响应 |

`502`/`503`/`504` 的响应体都写明**失败层级**，不用再猜是哪一跳出问题：

```
502 Bad Gateway

The tunnel is registered, but the local service behind it is unavailable.

  failed layer:  local service (the node owning the tunnel)
  node:          home
  tunnel:        app.example.com
  service:       web
  target:        127.0.0.1:8080
  cause:         Failed to connect to local TCP service 127.0.0.1:8080: Connection refused

No downstream node is involved: this node owns the tunnel and dialled the target above directly.
Start the local service, or fix its target.
Request ID: 0123456789abcdef
```

- `failed layer=local service (the node owning the tunnel)`：路由就在该节点上，是它连不上自己的 `local_addr`。
- `failed layer=local service (downstream node)`：节点和隧道都在线，是节点背后的服务连不上；`target` 就是该节点的 `local_addr`。
- `failed layer=tunnel (downstream node)`：路由还在，但下游节点开不了流（会话关闭/达到上限/超时），不是本地服务的问题。
- `node`/`session` 指明是哪一跳、哪条 v4 会话；配合同一 `Request ID` 可以直接定位到该节点的日志。
- 中间节点（中继）生成的 `502` 中，`node` 是**拥有该隧道的节点**，不是入口节点；入口只负责转发这条响应。

管理 API 的错误是 JSON：`{"error":"...","request_id":"..."}`；未知的 `/api/*` 路径还会在 `error` 中写明方法和路径。**`ht` 不会返回 `{"message":"page not found"}`**：如果看到这种响应，且响应头里没有 `x-http-tunnel-request-id`，说明它来自前置反向代理、CDN、托管平台或本地服务，而不是 `ht`。

如果这种响应只在**并发升高时**出现、过一段时间又消失，优先检查前置层是否把不同 `Host` 的请求复用到同一条到 `ht` 的 keep-alive 连接（见上一节的注意事项）。

`404` 的日志会带上**当时的路由表内容**和**请求本身**，可以直接看出“谁在请求、表里有什么、缺什么”，不用再去翻变更历史：

```
INFO http_tunnel::http: No route matches the host; answered 404
     request_id=a472f3bb090d3ee3 host=safeline.example.com
     method=GET target=/ user_agent=SafeLine-Health/1.0
     local_proxies=1 v4_routes=1 v4_sessions=1/256
     known_hosts_total=2
     known_hosts=["app.example.com (local:web)", "svc.example.com (v4:3)"]
```

- `method`/`target`/`user_agent`：能区分“真人访问了错误地址”和“某个服务的周期性健康检查/扫描”，后者通常只请求 `/`。
- `known_hosts_total=0` 且 `v4_sessions=0/…`：没有任何下游节点在线。
- `known_hosts` 里没有该域名：`ht` 上根本没有这条路由，检查 `[[proxies]]`、热加载是否成功和前置代理的转发目标。
- `v4_sessions` 有值但没有该域名：节点在线，但它广告的是别的域名（配置或冲突）。
- 同一行还出现 `withdrawn_prefix`：这条路由刚刚被撤销，通常是所属隧道在重连。

默认日志级别是 `info`，只记录路由失败（`404`/`502`/`504` 等）和会话事件。要看每个访客的路由决策（匹配到哪个服务、转发到哪个地址、v4 流的开启与结束），用 `RUST_LOG` 提高级别，无需重新编译：

```sh
RUST_LOG=debug ./ht -c ht.toml          # 全部 debug
RUST_LOG=http_tunnel=debug ./ht -c ht.toml  # 只开 ht 自己的 debug
docker run ... -e RUST_LOG=debug ghcr.io/jiangood/ht:latest
```

多跳时，同一个 `request_id` 会写进 v4 `OpenStream` 的 `trace_id`，中继把它转给自身嵌入式监听器时会放在内部请求头里，嵌入式服务继续沿用。因此入口、每个中继、叶子节点的日志，以及访客看到的错误页，都是同一个 ID，一次 `grep` 就能走完整条链路。

## Docker

仓库提供可直接使用的 compose 示例，镜像统一为 `ghcr.io/jiangood/ht:latest`，角色由挂载的 `ht.toml` 决定：

| 角色 | compose | 配置 | Dockerfile |
| --- | --- | --- | --- |
| 服务端 | [compose.yaml](./examples/docker/server/compose.yaml) | [ht.toml](./examples/docker/server/data/ht.toml) | [Dockerfile](./examples/docker/server/Dockerfile) |
| 客户端 | [compose.yaml](./examples/docker/client/compose.yaml) | [ht.toml](./examples/docker/client/data/ht.toml) | [Dockerfile](./examples/docker/client/Dockerfile) |
| Docker Socket | [compose.yaml](./examples/docker/client-docker-sock/compose.yaml) | [ht.toml](./examples/docker/client-docker-sock/data/ht.toml) | [Dockerfile](./examples/docker/client-docker-sock/Dockerfile) |

完整部署、验证和排查说明见 [examples/docker/README.md](./examples/docker/README.md)。

容器固定读取 `/app/ht.toml`，示例把配置放在各自的 `data/` 目录并挂载到 `/app`，修改后约 1 秒自动热加载。`local_addr` 在容器内解析：`127.0.0.1` 是容器自己，宿主机服务用 `host.docker.internal:<端口>`，同一 compose 的服务用服务名。停止时 `ht` 最多 drain 30 秒，compose 已设置 `stop_grace_period: 40s`（`docker run` 用 `--stop-timeout 40`）。

## 许可证

[Apache-2.0](./LICENSE)
