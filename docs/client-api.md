# 客户端 Web 管理与 API

客户端的 Web 管理页面和 REST API 共用一个端口，默认监听 `8610`，地址由 `web_host` 决定（默认 `0.0.0.0`）。在 `htc.toml` 中设置 `web_port = 0` 可关闭整个管理服务；设置 `web_host = "127.0.0.1"` 可只允许本机访问。`HTC_WEB_HOST` 与 `HTC_WEB_PORT` 可从环境变量覆盖这两项。

浏览器访问 `http://客户端地址:8610/`。浏览器弹出 HTTP Basic 认证时，用户名填写 `htc`，密码填写客户端配置中的 `token`。管理页面只使用原生 HTML 表单，不包含 CSS 或 JavaScript。

管理服务默认绑定到 `0.0.0.0`，可从网络访问。请使用防火墙、私有网络或带 TLS 的反向代理限制访问，不要直接暴露在公网；也可以设置 `web_host = "127.0.0.1"` 只允许本机访问。

## 页面功能

- 查看客户端名称、服务端地址、连接状态和隧道数量
- 列出已有隧道
- 新增隧道；使用已有名称可更新其域名和本地地址
- 删除隧道

通过页面进行的修改会校验配置、写入 `htc.toml`，并立即应用，无需重启客户端。配置文件必须可写。

如果隧道来自 `HTC_TUNNELS` 环境变量，页面和 API 对隧道的修改会被拒绝（`409 Conflict`）：环境变量只影响运行时配置，不会写回 `htc.toml`，修改后需要重启客户端。环境变量覆盖的标量配置（`HTC_SERVER`、`HTC_TOKEN` 等）也不会被写入文件。

## REST API

原有 API 路径与 Bearer Token 认证保持不变。每个请求都需要：

```http
Authorization: Bearer <token>
```

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/api/status` | 客户端名称、传输模式（`ws`/`wss`）、服务端地址、连接状态和隧道数量 |
| `GET` | `/api/tunnels` | 列出隧道 |
| `PUT` | `/api/tunnels/{name}` | 新增或更新隧道，请求体为 `{ "domain": "...", "local_addr": "..." }` |
| `DELETE` | `/api/tunnels/{name}` | 删除隧道 |

例如：

```bash
TOKEN=replace-with-your-client-token
BASE=http://127.0.0.1:8610

curl "$BASE/api/status" -H "Authorization: Bearer $TOKEN"
curl "$BASE/api/tunnels" -H "Authorization: Bearer $TOKEN"
curl -X PUT "$BASE/api/tunnels/nas" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"domain":"nas.example.com","local_addr":"127.0.0.1:80"}'
curl -X DELETE "$BASE/api/tunnels/nas" -H "Authorization: Bearer $TOKEN"
```

## 安全注意事项

客户端共享令牌同时用于连接服务端和管理 API；任何持有该令牌的人都能修改客户端隧道。妥善保管令牌，并限制管理端口的网络访问。
