# Protocol v4

Status: **implemented and shipped in `ht` 1.0**. v4 is the only wire protocol
`ht` speaks. This document describes the protocol as it is implemented, not a
future design proposal.

## Goals

- One WebSocket session per peer, multiplexing every route update and every
  visitor stream.
- Batch route advertisement and withdrawal with an explicit acceptance or
  rejection per prefix.
- A raw byte pipe for each visitor, so HTTP keep-alive, request bodies, chunked
  responses and WebSocket upgrades all pass through unchanged.
- Explicit limits on routes, streams and frame sizes.

## Entry point

The session endpoint is:

```text
/_http-tunnel/v4/connect
```

It is available only on the reserved `control_host` of the listener. There is no
version preamble and no fallback: any other path on that Host is either the
management interface or a `404`.

The upgrade request must carry:

```text
Authorization: Bearer <token>
X-HTTP-Tunnel-Node: <node_id>
```

The bearer token is checked before the WebSocket upgrade, so an unauthenticated
peer never reserves a session slot. The node id is a non-empty identifier used
for logging and for the proof below; it is not a trust boundary on its own.

## Handshake

```text
client                                            server
  |  GET /_http-tunnel/v4/connect                  |
  |  Authorization: Bearer <token>                 |
  |  X-HTTP-Tunnel-Node: <node_id>                 |
  |----------------------------------------------->|
  |  101 Switching Protocols                       |
  |<-----------------------------------------------|
  |  Hello { version: 4, node_id, role, caps }     |
  |----------------------------------------------->|
  |  Challenge { server_id, nonce }                |
  |<-----------------------------------------------|
  |  Auth { proof }                                |
  |----------------------------------------------->|
  |  Welcome { session_id }                        |
  |<-----------------------------------------------|
  |  Advertise { generation: 1, routes: [...] }    |
  |----------------------------------------------->|
  |  RouteAck { accepted: [...], rejected: [...] } |
  |<-----------------------------------------------|
```

- `role` is advisory for logging. Routing and authorization use the token and
  the advertised prefixes, not the role.
- `proof = SHA-256(token || nonce || node_id || server_id)`. The server id is
  its reserved `control_host`; the nonce is a fresh 32-byte random value per
  session.
- A wrong version, an empty node id, a malformed frame or a bad proof ends the
  handshake with an `Error` frame.
- The server takes a session permit only after a successful `Auth`. A global
  `max_pending_handshakes` budget bounds unauthenticated and slow upgrades.

## Framing

One `SessionFrame` is big-endian length-prefixed (`u32`) and bincode-encoded.
Several frames may share one WebSocket binary message. A single encoded frame
may not exceed 1 MiB; stream data frames are capped at 64 KiB so a large
transfer can interleave with control frames.

```rust
enum SessionFrame {
    // Handshake
    Hello { version: u16, node_id: String, role: Role, capabilities: u32 },
    Challenge { server_id: String, nonce: Vec<u8> },
    Auth { proof: Vec<u8> },
    Welcome { session_id: u64 },

    // Routes
    Advertise { generation: u64, routes: Vec<RouteAdvert> },
    Update { generation: u64, add: Vec<RouteAdvert>, remove: Vec<String> },
    RouteAck { generation: u64, accepted: Vec<String>, rejected: Vec<RouteRejection> },

    // Streams
    OpenStream { stream_id: u32, domain: String, header: Vec<u8>, trace_id: Vec<u8>, hop: u8 },
    StreamData { stream_id: u32, data: Vec<u8>, fin: bool },
    StreamWindowUpdate { stream_id: u32, credit: u32 },
    StreamReset { stream_id: u32, reason: StreamError },

    // Session maintenance
    Ping { seq: u64 },
    Pong { seq: u64 },
    GoAway { reason: String, drain_seconds: u32 },
    Error { code: ErrorCode, message: String },
}
```

`RouteAdvert` carries the exact domain or `*.suffix` prefix, the advertising
node's id, and a hop count. The current implementation sets `origin` to the
local node id and advertises every route with `hops = 0`; ACLs and
protocol-level loop detection are not part of 1.0.

## Route advertisement

- On session start the peer sends a full `Advertise` snapshot for generation 1.
- Afterwards it sends `Update` frames with additions and removals. The
  generation is monotonic.
- The server applies each snapshot or update and answers with `RouteAck`.
- A rejected prefix is logged once and is not retried until the advertised set
  changes again.

Current rejection reasons:

| Reason | Meaning |
| --- | --- |
| `InvalidPrefix` | The prefix is not a valid exact or `*.suffix` domain |
| `ReservedHost` | The prefix overlaps the server's reserved `control_host` |
| `TooManyRoutes` | The session would exceed `MAX_ROUTES_PER_SESSION` (256) |
| `Conflict` | Another session already owns the prefix |

Route lookup prefers an exact prefix and then the longest matching wildcard
suffix. Local `[[proxies]]` are checked before downstream session routes.

When a session ends, every route it still owns is withdrawn. A rejected route
never interrupts the process; the peer logs it and keeps serving its local
routes.

## Streams

The upstream side opens a stream for each visitor. The downstream side connects
to the `local_addr` of the matched route and pipes bytes in both directions.

- `OpenStream.header` carries the visitor's prefetched HTTP request head. The
  downstream applies the route's `auth_header`, if configured, before
  connecting.
- `StreamData { fin: true }` half-closes that direction. A stream ends when both
  directions are finished or one side resets it.
- A downstream that cannot reach its `local_addr` answers
  `StreamReset { ConnectFailed }`; the server answers the visitor with `502`.
- A stream whose session dies is reset, so the visitor receives `502`/`503`/`504`
  instead of hanging.
- The downstream never opens visitor streams. A relay is both a downstream and
  an upstream process, using one session in each direction.

### Flow control

Each stream has an initial send window of 256 KiB in each direction. The
receiver consumes data and sends `StreamWindowUpdate`; a sender stops at zero
credit. This bounds per-stream buffering and prevents one large transfer from
buffering the whole session.

A session may run at most 256 streams. The session's writer queue is bounded;
when it fills, the stream reader is backpressured instead of buffering without
limit.

## Keepalive and shutdown

- The WebSocket bridge sends a protocol-level ping every `keepalive_interval`
  seconds (15 by default) and gives up after three intervals without any inbound
  frame.
- `Ping`/`Pong` frames are also supported at the session layer.
- On shutdown the process sends `GoAway`, stops opening streams, withdraws its
  advertised routes, and drains in-flight visitors for up to 30 seconds.
- A lost session removes all routes owned by that session and resets its
  streams. Reconnecting sends a fresh full snapshot with a new generation.
- A rejected token is fatal: the process reports it and exits instead of
  retrying forever.

## Not in 1.0

The protocol reserves fields for features that are not implemented yet:

- per-node tokens and domain ACLs;
- protocol-level hop enforcement and loop detection;
- stream sharding to avoid head-of-line blocking;
- a drain window on `GoAway` (`drain_seconds` is currently `0`);
- stream resumption after a session loss.

These are roadmap items, not behavior to rely on.

## Implementation map

- `src/protocol_v4.rs`: frame types and the length-prefixed bincode codec.
- `src/session_server.rs`: handshake, session loop, route table, streams and
  flow control.
- `src/session_client.rs`: handshake, route snapshots/updates, streams to
  `local_addr`, reconnect backoff.
- `src/http.rs`: the v4 entry point and visitor-to-stream routing.
- `src/transport/websocket.rs`: the WebSocket byte-stream bridge, ping/pong and
  the upgrade request.
- `src/node.rs`: the universal router: embedded server plus one optional
  upstream session.
- `src/server.rs`: shared state, dynamic proxy list, activity drain and shutdown.
- `src/management.rs` / `src/management_api.rs`: runtime proxy management and
  the web page/API.
- `src/config_env.rs`: `ht.toml` loading and the `HT_*` environment overlay.
