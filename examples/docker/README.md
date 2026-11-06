# Docker 部署示例

三个目录分别对应最常见的容器角色，`compose.yaml` 直接使用发布镜像，不包含构建步骤：

| 角色 | compose | 配置 | 说明 |
| --- | --- | --- | --- |
| 服务端 | [server/compose.yaml](./server/compose.yaml) | [server/data/ht.toml](./server/data/ht.toml) | 公网入口，发布 `7777` |
| 客户端 | [client/compose.yaml](./client/compose.yaml) | [client/data/ht.toml](./client/data/ht.toml) | 内网节点，只向上游拨出 |
| Docker Socket | [client-docker-sock/compose.yaml](./client-docker-sock/compose.yaml) | [client-docker-sock/data/ht.toml](./client-docker-sock/data/ht.toml) | 客户端挂载 `/var/run/docker.sock`，把 Docker API 暴露到隧道 |

镜像统一为 `ghcr.io/jiangood/ht:latest`，角色完全由 `ht.toml` 决定。正式部署建议用 `HT_IMAGE` 固定版本，例如 `HT_IMAGE=ghcr.io/jiangood/ht:4.0.1`。

## 服务端

```sh
cd examples/docker/server
# 修改 data/ht.toml 里的 domain、token
docker compose up -d
```

服务端的 `[server].domain` 是保留 Host，不能与任何业务域名相同；客户端会用它做 v4 握手校验。容器内必须保持 `bind = "0.0.0.0"`，否则端口映射无法访问。只允许本机 Nginx/Caddy 反代时，把 compose 的端口改成 `"127.0.0.1:7777:7777"`。

管理页地址是 `http://<server.domain>:7777/`，用户名 `ht`，密码为 `[server].token`。域名尚未解析时：

```sh
curl -i -H "Host: control.example.com" http://127.0.0.1:7777/
```

## 客户端

```sh
cd examples/docker/client
# 修改 data/ht.toml：proxies、local_addr、[upstream].url 和 token
docker compose up -d
```

客户端不写 `[server]`，因此不监听任何端口，也不需要发布端口。`[upstream].url` 的主机名必须与入口的 `[server].domain` 完全一致；公网部署使用 `wss://control.example.com:443`，本机联调才用 `ws://...:7777`。

`local_addr` 由**发起连接的容器**解析：

| 目标 | `local_addr` |
| --- | --- |
| 同一 compose 中的另一个服务 | 服务名，如 `api:8080` |
| 宿主机上的服务（Docker Desktop） | `host.docker.internal:<端口>` |
| 宿主机上的服务（Linux Engine） | `host.docker.internal:<端口>`，并保留 compose 里的 `extra_hosts` |
| 宿主机上的 Unix Socket | 挂载进容器后用 `unix:///var/run/docker.sock` |

容器里的 `127.0.0.1` 是容器自己，不是宿主机。把 `local_addr` 写成 `127.0.0.1:3000` 却访问不到宿主服务时，访客会收到 `502`。

客户端没有管理页。修改 `data/ht.toml` 后约 1 秒自动热加载，不需要 `docker compose restart`。

### 本机联调

在同一个 Docker 主机上跑服务端和客户端时，让两边的 Host 保持一致。例如把服务端 `data/ht.toml` 改为：

```toml
[server]
domain = "host.docker.internal"
```

再把客户端 `data/ht.toml` 的 `[upstream]` 改为：

```toml
[upstream]
url = "ws://host.docker.internal:7777"
token = "与服务端 token 相同"
```

如果保留 `control.example.com`，就需要让客户端容器能把这个域名解析到服务端，并确保 `url` 的主机名与 `[server].domain` 相同。

## Docker Socket

`client-docker-sock` 把宿主机的 Docker Socket 挂进 ht，再用一条普通的 `[[proxies]]` 代理 Docker API：

```toml
[[proxies]]
name = "docker"
domain = "docker.example.com"
local_addr = "unix:///var/run/docker.sock"
x-auth = "demo-visitor-token"
```

```sh
cd examples/docker/client-docker-sock
docker compose up -d
```

验证（把地址换成入口所在机器）：

```sh
curl -sS \
  -H "Host: docker.example.com" \
  -H "X-Auth: demo-visitor-token" \
  http://<入口地址>:7777/version
```

返回 `ApiVersion` / `MinAPIVersion` 就说明 Docker API 已经穿过隧道。`x-auth` 校验通过后，`X-Auth` 会在转发给 Docker daemon 之前被移除。

Docker CLI 也可以直接连：

```sh
export DOCKER_HOST=tcp://docker.example.com:7777
export DOCKER_CUSTOM_HEADERS="X-Auth=demo-visitor-token"
docker ps
```

这里假设入口直接发布了 `[server].port`（默认 `7777`）。如果入口在 TLS 终止层后面，Docker CLI 不能把明文 HTTP 发到 443 的 HTTPS 端口，需要改用 TCP/TLS 直通、VPN 或内网入口。

`DOCKER_CUSTOM_HEADERS` 需要 Docker 27.1 及以上。更早的版本使用独立配置目录：

```json
// ~/.docker-tunnel/config.json
{"HttpHeaders":{"X-Auth":"demo-visitor-token"}}
```

```sh
export DOCKER_CONFIG="$HOME/.docker-tunnel"
export DOCKER_HOST=tcp://docker.example.com:7777
docker ps
```

> Docker API 等价于宿主机 root。公网部署必须加 TLS 并设置 `x-auth`，最好只通过 VPN、WireGuard 或内网访问；不要把未加认证的入口直接暴露到公网。

### Windows / Docker Desktop

Windows 原生 `ht.exe` 不支持 Unix Socket（`npipe://` 也会在配置校验时被拒绝），因此让 ht 以 Linux 容器运行，直接挂载 socket 即可，不需要开启 `2375`，也不会新增任何监听端口。

如果确实要让原生 `ht.exe` 走 TCP，可在 Docker Desktop 的 Settings → General 勾选 **Expose daemon on tcp://localhost:2375 without TLS**，确认它只监听回环：

```powershell
Get-NetTCPConnection -LocalPort 2375 -State Listen | Select-Object LocalAddress, LocalPort
# 应为 127.0.0.1，而不是 0.0.0.0
```

然后把客户端配置改成：

```toml
[[proxies]]
name = "docker"
domain = "docker.example.com"
local_addr = "host.docker.internal:2375"
x-auth = "demo-visitor-token"
```

Windows 原生 `ht.exe` 不在容器里时，`local_addr` 直接用 `127.0.0.1:2375`。

## 使用 Dockerfile

compose 示例不构建镜像，直接使用发布镜像。每个目录里的 `Dockerfile` 是可选的“把配置烘进镜像”方式：

```sh
docker build -t ht-server examples/docker/server
docker run -d --name ht-server --restart unless-stopped \
  --stop-timeout 40 -p 7777:7777 ht-server
```

客户端同理，把目录换成 `examples/docker/client` 或 `examples/docker/client-docker-sock`，并且不要发布端口。烘进镜像的配置在容器重建后回到初始值；需要管理页持久化保存时，仍然挂载一个可写目录到 `/app`。

## 通用注意事项

- 容器固定读取 `/app/ht.toml`，示例把配置放在 `data/` 并挂载到 `/app`。保存管理页修改需要对 `/app` 可写；只读运行时可把挂载设为 `:ro`，只通过宿主机编辑配置。
- `ht` 收到 `SIGTERM` 后最多 drain 30 秒。compose 已设置 `stop_grace_period: 40s`；`docker run` 需要 `--stop-timeout 40`，否则在途连接会被 Docker 的默认 10 秒超时截断。
- 端口映射必须与 `[server].port` 一致。管理页和 API 共用这个端口，不需要额外映射；纯客户端不需要发布端口。
- 镜像基于 `scratch`，没有 shell 和 `curl`。用 `docker logs <container>` 查看日志，验证请求从宿主机或另一个容器发出。
- 默认以 root 运行。改成 `--user 65532:65532`（compose 的 `user:`）时，要保证挂载目录对该 UID 可写；挂载 `docker.sock` 时还要处理 socket 的组权限。
- 镜像没有内置 `HEALTHCHECK`，对 `[server].port` 做 TCP 检查即可。

## 排查

| 现象 | 检查 |
| --- | --- |
| `404 No tunnel for this host` | 客户端是否已连上；域名是否在某个 `[[proxies]]` 中 |
| v4 会话返回 `401` | 两端 Token 是否一致 |
| WebSocket 握手失败 | `[server].domain` 与 `[upstream].url` 的主机名是否一致；TLS 终止层是否转发 `Upgrade` / `Connection` |
| `502`，`cause` 为 `Connection refused` | `local_addr` 在容器内是否可达；是不是误用了 `127.0.0.1` |
| `502`，`cause` 为 `no such host` | `host.docker.internal` 是否配置；Linux 需要 `extra_hosts` |
| 容器停止时请求被截断 | `stop_grace_period` / `--stop-timeout` 是否设为 40 秒 |
