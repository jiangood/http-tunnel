# Internals

## Conceptions

### Tunnel
The entity that maps a domain to the traffic that needs to be forwarded.

### Server
The host that runs `http-tunnel` in the server mode. It reads `hts.toml` for its
HTTP port, shared token, `server_host` and limits, and it learns the routes of
its clients from the v4 sessions they open.

### Client
The host behind the NAT that runs `http-tunnel` in the client mode. It declares
its own tunnels in a configuration file: each tunnel maps a domain to a local
service. One v4 session carries every tunnel of the client.

### Node
A relay hop that runs `htn`. It embeds a *server* (it listens for visitors and
downstream peers) and one upstream v4 session (it advertises the routes of its
downstream peers and local proxies). It is how a chain of isolated network
segments is built with one process per hop; a node without `[upstream]` is an
ingress.

### Visitor
Who visits a *tunnel*, via the *server*.

### Session
One WebSocket between a *server* and a *client* or *node*, carrying the v4
frames. It replaces the per-tunnel control and data channels of v3. The session
announces the peer identity, advertises routes as a snapshot and diffs, and
multiplexes every visitor stream.

### Stream
A logical byte pipe inside a session, one per visitor. The upstream server opens
a stream with the resolved domain and the visitor's prefetched HTTP header; the
downstream connects to the route's `local_addr` and pipes bytes. Streams have
half-close, a reset reason, and a per-direction credit window.

## The process

When `htc` starts, it reads its configuration file: a list of *tunnels*, each an
HTTP domain mapped to a `local_addr` on the client (a tunnel also carries a
`name`, its identity within the client). Every setting also has an `HTC_*`
environment counterpart that wins over the file, which lets a container run
without a mounted `htc.toml`. The client opens one v4 session to the server's
`server_host:server_port` and advertises its tunnels.

The upgrade carries the shared token in `Authorization: Bearer`; the session then
challenges the peer with a nonce and checks a proof bound to the node id and the
server id. Once authenticated, the server installs the advertised prefixes in
its routing table. A route is removed when the session closes or sends an update.

The server listens on `server_port` for HTTP visitors. When a visitor connects, it
reads the request line and the `Host` header, looks up the session that serves
the domain, and opens a stream. The visitor's prefetched header is sent in the
`OpenStream` frame; the downstream connects to its `local_addr`, applies the
tunnel's `auth_header` if any, writes the header and pipes the rest. The response
comes back over the same stream.

Each stream has an initial window (256 KiB) in each direction. The receiver sends
`StreamWindowUpdate` as it consumes bytes, so a slow local service pauses the
sender instead of buffering without bound. Stream data frames are 64 KiB, small
enough to interleave interactive streams on one session. A session is still one
ordered WebSocket, so a large transfer can add some head-of-line delay; this is
the tradeoff for replacing hundreds of sockets with one.

A stream whose downstream cannot reach its `local_addr` is reset with
`ConnectFailed`; the server answers the visitor with `502`. A stream whose session
dies is closed and the visitor gets a `503`/`504` instead of hanging. A domain is
served while its session is connected; a duplicate advertisement from another
session is rejected with `Conflict` and logged.

Each WebSocket bridge sends a protocol ping every `keepalive_interval` seconds
(15 by default) and gives up after three intervals without any inbound frame, so
a silently dropped session is detected within 45 seconds. The v4 session relies
on that keepalive; there is no separate application heartbeat.

On shutdown, after the HTTP listener stops accepting, the server waits for the
in-flight visitors (up to `DRAIN_TIMEOUT`, 30 seconds) before it closes the
sessions, so a graceful restart doesn't cut the flows off. The `ActivityTracker`
counts the visitors that are still being served for that.

A *node* composes the two roles in one process. Its upstream session forwards
visitors to `127.0.0.1:<server_port>`, where the embedded server routes them by
`Host` again: the data plane is recursive because a stream is a raw byte pipe and
the `Host` header is never rewritten. On shutdown a node closes its upstream
session before the embedded server drains, so no new visitor is routed to it
while the in-flight ones finish.

## Protocol v4

The frames, route advertisement, stream flow control, identity and the remaining
work are described in [protocol-v4.md](./protocol-v4.md). v3 is not supported;
all three binaries upgrade together.
