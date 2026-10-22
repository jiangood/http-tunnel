# WebSocket 传输代码路径审查

> 分支：`feat/websocket-transport`  
> 状态：阶段 0 审查完成；阶段 1 原型已实现，GitHub Actions 的 check、test、build 和进程级 E2E 均通过。

## 1. 现有连接流程

### 服务端

- `src/server.rs::Server::run` 在 `server_port` 上监听客户端连接。
- 每条连接交给 `handle_connection`，先用 `protocol::read_payload` 读取 `ClientHello`，再分别进入控制通道或数据通道握手。
- 控制通道完成 token + nonce 摘要校验后注册路由，并由现有控制通道句柄维护心跳和数据通道池。
- `src/http.rs::serve` 单独监听 `http_port`，`handle_visitor` 读取 HTTP 请求头中的 Host，再转发给客户端隧道或服务端本地代理。

### 客户端

- `src/client.rs::ControlChannel::run` 连接 `remote`，执行现有控制握手和认证。
- 收到 `CreateDataChannel` 后，客户端创建新的连接；数据连接先发送 `ClientHello::Data`，收到 `StartForwardTcp` 后把数据转发到本地目标。
- 现有控制连接与数据连接均依赖同一套长度前缀 bincode 协议，但它们是独立的 TCP 连接。

### 现有 WebSocket 能力的边界

当前 HTTP 入口支持转发业务请求中的 WebSocket/HTTP Upgrade。这不代表客户端到服务端的控制连接已支持 WebSocket；新增传输模式需要单独实现。

## 2. 建议的最小侵入实现

**让每条现有逻辑 TCP 连接对应一条 WebSocket 连接，并在两端把 WebSocket binary message 桥接成异步字节流。**

- 客户端使用 `ws://<control-host>:<http-port>/_http-tunnel/v1/connect` 建立连接。
- 服务端 HTTP 入口只在配置的控制 Host 和专用路径上接受 WebSocket Upgrade。
- 服务端将 WebSocket binary message 与一条连接到本机 `server_port` 的 TCP stream 双向桥接。
- 这样服务端原有的 `handle_connection`、协议握手、认证、会话管理和数据通道池都可以保留。
- 客户端增加统一的异步字节流抽象：TCP 模式使用原始 TCP stream；WebSocket 模式用后台任务在 WebSocket binary message 与 `tokio::io::DuplexStream` 之间搬运字节。
- 控制连接和每条数据连接各自建立 WebSocket；第一版不做多路复用，不改变现有协议帧，也不合并控制/数据通道。

该设计会增加每条逻辑连接的 WebSocket 帧封装与桥接任务开销，但比重写现有协议和服务端会话模型更容易验证，也最有利于保持 TCP 模式不变。它仍要求 hts 在本机绑定原有 server_port（默认 2333）；该端口可以在防火墙/安全组中禁止公网访问，但若运行环境禁止进程本地绑定该端口，就需要下一阶段改为直接接入共享的内部连接处理器，而不是 loopback bridge。

## 3. 配置与路由建议

- 服务端新增可选 `control_host`，默认未配置时完全保持旧行为。
- 客户端新增可选 `transport`，缺省为 `tcp`；WebSocket 模式用 `remote` 中的主机与 HTTP 端口作为连接目标，避免同时维护两个地址字段。
- 约定 `transport = "websocket"` 时，客户端将 `remote` 解释为控制 Host 的 `host:port` 地址，并使用专用 WebSocket 路径。
- 控制 Host 必须与业务 Host 区分；Host 只用于路由，仍必须执行现有 token 认证。
- 服务端处理 WebSocket 请求前必须检查 Host 与路径。其他 Host 的普通 HTTP 业务请求继续走原路由。
- 第一版使用 `ws://`，不会自动申请证书；文档必须明确它是明文传输，公网敏感流量不应在未加密情况下使用。

## 4. 依赖与验证

- 当前项目没有 WebSocket 协议库依赖。实现需要引入 Tokio 兼容的 WebSocket 库和 futures 工具。
- `.github/workflows/rust-checks.yml` 会执行 `cargo check --all-features`、`cargo test --all-features`、构建全部二进制及进程级 E2E 测试，因此实现必须更新锁文件并通过完整 CI。
- 当前执行环境无法直接访问 GitHub 或运行仓库的 Cargo 测试；代码提交后应以 GitHub Actions 的实际结果为准，不应宣称本地已验证。

## 5. 已落地的首版原型

- 已增加可选客户端 transport 配置（缺省仍为 TCP）和服务端可选 control_host。
- 已增加 WebSocket 与异步字节流之间的桥接；服务端 WebSocket 会桥接到现有 server_port TCP listener。
- 已将客户端控制连接与数据连接统一为字节流接口，WebSocket 模式下每条逻辑连接独立建立 WebSocket。
- 已在 HTTP 入口按 control_host + 专用路径分流，并在 README 中记录配置方式和明文传输风险。
- 已在 GitHub Actions 中通过编译、单元/集成测试、二进制构建及进程级 E2E；仍属于实验性原型，尚未覆盖所有阶段 2 的异常、背压与恢复场景。

## 6. 阶段 2 追加的资源与生命周期保护

- WebSocket 帧和消息最大 1 MiB，避免单条消息无界分配。
- WebSocket 握手超时 10 秒，本地 TCP 上游连接超时 5 秒。
- HTTP 入口最多允许 256 条活动 WebSocket 连接；达到上限时返回 503。
- WebSocket 传输连接不计入业务 visitor 的 drain 计数，避免服务端优雅关闭时等待长连接超时；真正的业务 visitor 仍由原 activity guard 统计。
- 拒绝把保留的 control_host 注册为业务隧道，避免控制域名被业务路由覆盖。

## 7. 下一阶段任务

1. 增加可选配置字段并保证旧配置默认仍使用 TCP。
2. 增加 WebSocket byte-stream adapter 和依赖。
3. 在 HTTP 入口按 Host + path 接入 WebSocket，并桥接至本机 TCP listener。
4. 在客户端统一连接类型，支持 TCP / WebSocket 两种方式。
5. 补配置解析、Host/path 拒绝、连接桥接及端到端测试。
