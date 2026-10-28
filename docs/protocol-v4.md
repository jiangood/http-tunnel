# Protocol v4 — Design Proposal

Status: **implemented for `ht`**. v4 is the only protocol `ht` speaks; the v3
implementation has been removed. This document describes the protocol as it is
implemented and the parts that are still pending.

The issue that introduced node mode leaves v4 deliberately open: it is only worth
doing when multi-tenant relays, large route sets, or loop-proof topologies are
actual requirements. This proposal is the decision input for that step.

## Why v4

Protocol v3 has one WebSocket per control channel and one per data channel:

- A mirrored route costs one control channel, plus `pool_min` warm data channels
  on the hop above. A relay with 300 domains is hundreds of sockets.
- Route changes after a reconnect are re-sent by opening and closing individual
  control channels. There is no batch advertisement or withdrawal.
- A domain that the upstream permanently rejects (for example because it overlaps
  `server_host`) is retried forever without a reason.
- There is no node identity beyond the shared token, and therefore no per-node
  domain ACL.
- There is no hop count or protocol-level loop detection; only the local
  self-upstream check exists.
- A visitor has no trace id across hops, which makes 502/504 attribution hard.

v4 replaces the per-channel protocol with a **multiplexed session per peer** and
adds route advertisement, identity/ACL, hop limits and tracing.

## Goals

- One WebSocket session per peer, multiplexing many visitor streams.
- Batch route advertisement and withdrawal with explicit acceptance/rejection.
- Per-node identity and domain ACLs.
- Route origin and hop count in advertisements, with loop rejection.
- A trace id per visitor, carried in stream metadata.
- Keep the HTTP data plane unchanged: a stream is still a raw byte pipe to a
  registered `local_addr`, and `Host` is never rewritten.
- Break the old wire protocol: all three binaries upgrade together.

## Non-goals

- UDP, arbitrary outbound TCP, or a general forward proxy.
- End-to-end encryption between hops (TLS stays hop by hop).
- Mesh discovery or dynamic peering.
- Stream resumption after a session loss.
- Replacing HTTP routing / request logging.
- HTTP/2 or WebSocket-over-WebSocket changes for visitors.

## Entry point

v4 uses one entry point on the existing listener:

```
/_http-tunnel/v4/connect
```

The old `/_http-tunnel/v1/connect` path returns `404`. There is no version
preamble and no fallback: a v3 peer is not supported. The path keeps the
protocol version explicit in logs and lets the entrypoint reject old clients
without parsing their frames.

## Session handshake

```
client                                            server
  |  GET /_http-tunnel/v4/connect                  |
  |  Authorization: Bearer <token>                 |
  |  X-HTTP-Tunnel-Node: <node_id>                 |
  |----------------------------------------------->|
  |  101 Switching Protocols                       |
  |<-----------------------------------------------|
  |  Hello { version: 4, node_id, role, caps }     |
  |----------------------------------------------->|
  |  Challenge { server_id, nonce, limits }        |
  |<-----------------------------------------------|
  |  Auth { proof }                                |
  |----------------------------------------------->|
  |  Welcome { session_id, limits, trace? }        |
  |<-----------------------------------------------|
  |  Advertise { generation: 1, routes: [...] }    |
  |----------------------------------------------->|
  |  RouteAck { accepted: [...], rejected: [...] } |
  |<-----------------------------------------------|
```

- `X-HTTP-Tunnel-Node` lets the server select the per-node token before the
  upgrade, so an unauthenticated peer still cannot reserve a session slot. The
  header is repeated in `Hello`; a mismatch is a protocol error.
- `proof = SHA-256(token || nonce || node_id || server_id)` for the first
  iteration. It binds the session to the server's nonce and both identities.
  (HMAC-SHA-256 is preferable; it needs no new dependency because `sha2` is
  already present, but a small HMAC helper is required.)
- `role` is `client` (leaf) or `node`; it is advisory for logging, not for trust.

## Messages

One `SessionFrame` is length-prefixed (big-endian `u32`) and bincode-encoded,
like v3. Several frames may share one WebSocket binary message; stream data
frames are capped at 64 KiB so a large transfer interleaves with control frames.

Sketch (Rust enums):

```rust
enum SessionFrame {
    // Handshake
    Hello { version: u16, node_id: String, role: Role, caps: Capabilities },
    Challenge { server_id: String, nonce: [u8; 32], limits: SessionLimits },
    Auth { proof: [u8; 32] },
    Welcome { session_id: u64, limits: SessionLimits },

    // Routes
    Advertise { generation: u64, routes: Vec<RouteAdvert> },
    Update { generation: u64, add: Vec<RouteAdvert>, remove: Vec<String> },
    RouteAck { generation: u64, accepted: Vec<String>, rejected: Vec<RouteRejection> },

    // Streams (the server opens visitor streams)
    OpenStream { stream_id: u32, domain: String, trace_id: [u8; 16], hop: u8 },
    StreamData { stream_id: u32, data: Vec<u8>, fin: bool },
    StreamWindowUpdate { stream_id: u32, credit: u32 },
    StreamReset { stream_id: u32, reason: StreamError },

    // Session maintenance
    Ping { seq: u64 },
    Pong { seq: u64 },
    GoAway { reason: String, drain_seconds: u32 },
    Error { code: ErrorCode, message: String },
}

struct RouteAdvert { prefix: String, origin: String, hops: u8 }
struct RouteRejection { prefix: String, reason: RouteError }
```

`prefix` is an exact domain or a `*.suffix` wildcard, normalized exactly like v3.
`origin` is the node id that first published the route; `hops` is the number of
relay hops it has crossed.

## Route advertisement

- On session start the peer sends a **full snapshot** for generation 1.
- Afterwards it sends `Update` frames with additions and removals; the generation
  is monotonic and lets the server reject stale updates.
- The server applies the snapshot atomically: new prefixes are inserted, removed
  prefixes are deleted, and the rest is unchanged.
- Each prefix is answered in `RouteAck` as accepted or rejected with a reason:
  `AclDenied`, `ReservedHost`, `Conflict`, `TooManyRoutes`, `LoopDetected`,
  `HopsExceeded`, `InvalidPrefix`.
- The peer logs a rejection once and does not retry that prefix until its
  advertisement changes. There is no per-prefix control channel to retry.
- A node re-advertises the routes of its downstream sessions with `hops + 1` and
  the original `origin`. Its own proxies and local routes have `origin = self`
  and `hops = 0`.

Conflict policy is an open question. The recommendation is **reject the later
advertisement** (`Conflict`) by default, with an explicit
`allow_takeover = true` option that reproduces v3 semantics. Silent takeover is
the main reason a multi-tenant relay is unsafe today; the current prototype
rejects duplicates.

## Streams

The upstream side opens a stream for each visitor; the downstream side connects
to the `local_addr` of the matched route and pipes bytes.

- `OpenStream` carries the resolved `domain`, a random `trace_id` and the hop
  count. The visitor's HTTP header is sent as the first `StreamData` with
  `fin = false`; the stream is a raw byte pipe after that.
- `StreamData { fin: true }` half-closes that direction, preserving HTTP request
  bodies, chunked responses and WebSocket upgrades. A stream ends when both
  directions are finished or one side resets it.
- A downstream that cannot connect to its `local_addr` answers
  `StreamReset { ConnectFailed }`; the server answers the visitor with `502`
  immediately. This replaces the v3 heuristic of opening a data channel, closing
  it without writing, and answering `504`.
- `StreamReset` also covers timeouts, local IO errors and shutdown.
- The downstream never opens visitor streams; the role asymmetry stays (a
  downstream always dials its upstream). A node is both a downstream and an
  upstream, using two sessions in opposite directions.

### Flow control

Each stream has an initial window (default 256 KiB) and the session has a total
window. The receiver sends `StreamWindowUpdate` as it consumes bytes. A sender
stops at zero credit. This bounds memory in the mux and prevents one large
transfer from buffering the whole session.

### Head-of-line blocking

A single TCP/WebSocket session orders all streams. A 1 GiB transfer can delay
interactive streams on the same session. Mitigations:

- 64 KiB stream frames and round-robin scheduling between ready streams.
- Bounded per-stream outbound queues: a stream with a full queue stops reading
  its local socket, so backpressure reaches the origin.
- Optional sharding: `sessions = N` per peer, with routes assigned by hash. This
  restores some of v3's isolation at the cost of N sockets.

Whether sharding is needed is an open question; it is a deployment knob, not a
protocol change.

## Identity and ACL

Planned: the server gets an optional node table.

```toml
# ht.toml (planned)
[[nodes]]
id = "relay-a"
token = "per-node-secret"
allow = ["*.a.relay.example.com", "svc.example.com"]
max_hops = 8

[[nodes]]
id = "home"
token = "another-secret"
allow = ["home.example.com"]
```

- The upgrade's `X-HTTP-Tunnel-Node` selects the row; the bearer token must match
  that row. Until this is implemented, the session authenticates the shared token
  and the node id is only logged.
- A node may advertise only prefixes contained in its `allow` list. `*.a.example.com`
  allows `x.a.example.com`, but not `a.example.com` (the same apex rule as the
  routing table). An allowed wildcard covers deeper wildcards.
- `RouteAck` reports `AclDenied`; the node logs it and keeps serving locally.
- mTLS/certificate identity is out of scope; the token remains the credential.

## Hop count and loop detection

- Each route carries `origin` and `hops`; a node increments `hops` when it
  re-advertises a downstream route.
- A node rejects an advertisement whose `origin` is its own id (`LoopDetected`)
  or whose `hops` exceeds `max_hops` (`HopsExceeded`). A cycle that returns to
  its origin is therefore cut at advertisement time.
- `OpenStream.hop` is advisory metadata for logs. It cannot be carried through
  the node's loopback listener without changing the HTTP request, so per-request
  loop detection stays out of scope. The route-level check plus the existing
  self-upstream check cover the realistic misconfigurations.
- `max_hops` defaults to 8 and is configurable on the server.

## Tracing

- The ingress generates a random 16-byte `trace_id` per visitor and logs it with
  the request. It is carried in `OpenStream` across every v4 hop.
- `stream_id` identifies the stream within a session; sessions have a
  `session_id`.
- A downstream logs `trace_id` when it opens the local connection and when a
  stream fails.
- No HTTP headers are added or rewritten; trace ids stay in logs and protocol
  metadata.

## Session maintenance

- WebSocket ping/pong plus `Ping`/`Pong` frames detect a dead peer within three
  keepalive periods, as today.
- On shutdown the server sends `GoAway` and stops opening streams; existing
  streams drain for `drain_seconds` and are then reset. The client closes the
  session after the drain.
- A lost session removes all of its routes from the routing table and resets the
  streams. Visitors waiting for a stream get `502`/`503` instead of hanging.
- There is no stream resumption in v1. A reconnect sends a full route snapshot
  with a new generation.

## Migration

There is no mixed-version support. Every hop must be upgraded together to a
v4-compatible `ht`; an old peer gets `404` on the v4 entrypoint. The unified
configuration keys (`token`, `[[proxies]]`, `[upstream]`) describe the same
roles as before.

## Implementation map

- `src/protocol_v4.rs`: the frame types and length-prefixed bincode codec.
- `src/session_server.rs`: v4 handshake, session loop, route table, streams and
  flow control.
- `src/session_client.rs`: v4 handshake, route snapshots/updates, streams to
  `local_addr`, reconnect backoff.
- `src/http.rs`: the v4 entrypoint and visitor-to-stream routing.
- `src/client.rs`: management state for the running server's `[[proxies]]` and
  persistence to `ht.toml`.
- `src/client_env.rs`: the `HT_*` (and legacy `HTC_*`) environment overlay.
- `src/node.rs`: the universal router: embedded server plus one optional upstream
  session, whose advertised map is the route set or one claim.
- `src/server.rs`: the shared state, dynamic proxy list, activity drain,
  shutdown and the stream relay.
- `docs/`, `examples/`, `Dockerfile`/release scripts: v4 docs and examples.

## Test plan

- Unit: frame encode/decode, prefix containment, ACL matching, hop validation,
  generation ordering.
- Session: handshake/auth (good token, wrong token, node/token mismatch, replayed
  nonce), route snapshot + diff + rejection, GoAway.
- Streams: interleaved streams, half-close, reset on connect failure, flow
  control (a slow reader pauses the sender), large bodies, WebSocket upgrades.
- Node: 3-hop chain with v4 sessions, fan-out, route withdrawal on leaf stop,
  ingress restart and re-advertisement, loop rejection, ACL rejection.
- Process E2E: real `ht` processes for an ingress, a leaf, an env-only leaf and
  a mirroring relay chain.
- Soak: 1 000 streams, 1 000 routes, route churn.

## Implemented and pending

Implemented:

- one v4 session per peer; v3 is removed;
- route snapshots/diffs with accepted/rejected prefixes;
- exact/wildcard route lookup on both sides;
- visitor streams with half-close, reset on connect failure, initial windows and
  window updates;
- reconnect with a full snapshot, and a rejected token as a fatal error;
- the existing config keys, so the binaries upgrade together.

Pending:

- per-node identity and ACL (`[[nodes]]`; the session currently uses the shared
  token);
- route origin/hop enforcement (the fields exist, but the prototype advertises
  `hops = 0` and does not reject loops at the protocol level);
- stream sharding / head-of-line mitigation beyond 64 KiB frames and windows;
- a drain window on `GoAway` (the prototype closes streams immediately);
- soak and large-scale tests.

## Open questions

1. **Binary schema stability.** bincode is compact but not self-describing. Is a
   versioned field list enough, or should v4 use a self-describing format
   (postcard/CBOR) for forward compatibility?
2. **Conflict policy.** Reject duplicates by default (implemented) or allow
   explicit takeover? This changes multi-tenant semantics.
3. **Head-of-line blocking.** Is one session per peer enough, or should sharding
   be part of the next iteration?
4. **Hop enforcement.** Is advertisement-level loop detection sufficient, or
   must per-request hop counts be carried through the node's loopback listener
   (which would require either an internal header or an in-process handoff)?
5. **Warm stream pool.** v4 opens streams on demand; is a warm pool worth the
   extra state for latency-sensitive short requests?
6. **ACL source.** Static `[[nodes]]` only, or an external auth hook/resource?
7. **Timeout defaults.** Session idle, stream open timeout and drain deadline.

## Recommendation

The session core is the foundation; do not add more deployment features until
the ACL, hop enforcement and HOL answers are decided. The next step is to add
`[[nodes]]` ACLs and hop validation on top of the existing frames, then decide
whether sharding is needed.
