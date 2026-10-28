# Protocol v4 — Design Proposal

Status: **proposal, not implemented**. Stage 1 (node mode) and Stage 2 (route
mirroring) work on protocol v3. This document defines what a v4 would change,
what it would keep, and how a chain can migrate without breaking v3 peers.

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
- Keep protocol v3 working; v3 and v4 peers can coexist on one entrypoint.

## Non-goals

- UDP, arbitrary outbound TCP, or a general forward proxy.
- End-to-end encryption between hops (TLS stays hop by hop).
- Mesh discovery or dynamic peering.
- Stream resumption after a session loss.
- Replacing HTTP routing / request logging.
- HTTP/2 or WebSocket-over-WebSocket changes for visitors.

## Version negotiation

v3 and v4 use different session paths on the same listener:

```
/_http-tunnel/v1/connect    protocol v3 (unchanged)
/_http-tunnel/v4/connect    protocol v4 session
```

The path is versioned instead of the frame preamble because a v3 server decodes
the first frame as a bincode `ClientHello`; an unambiguous path lets an old
server answer `404` instead of misparsing a v4 frame. The server host and token
reservation logic is shared by both paths.

A v4-capable server accepts both paths. A v4 client configured with
`protocol = "auto"` tries v4 first and falls back to v3 only on a transport-level
`404`/`405` from the entrypoint (an old server). It does not fall back on an auth
failure or a protocol error.

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
- A v3 upgrade with no node header is handled by the v3 path exactly as today.

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
  advertisement changes. This replaces the v3 "permanent rejection retried
  forever" behaviour.
- A node re-advertises the routes of its downstream sessions with `hops + 1` and
  the original `origin`. Its own proxies and local routes have `origin = self`
  and `hops = 0`.

Conflict policy is an open question. The recommendation is **reject the later
advertisement** (`Conflict`) by default in v4, with an explicit
`allow_takeover = true` option that reproduces v3 semantics. Silent takeover is
the main reason a multi-tenant relay is unsafe today.

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

The server gets an optional node table:

```toml
# hts.toml (v4)
protocol = "auto"          # "v3", "v4" or "auto"

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
  that row. A v3 upgrade without the header uses the shared `token` and maps to
  an anonymous principal (no ACL, v3 behaviour).
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

## Compatibility and rollout

| Pair | Behaviour |
| --- | --- |
| v3 client ↔ v3 server | unchanged |
| v3 client ↔ v4 server | v3 path, shared token, no ACL unless configured for the shared principal |
| v4 client ↔ v3 server | `auto` falls back to v3 after a 404/405; `v4` fails fast |
| v4 client ↔ v4 server | full v4 |
| v4 node ↔ v4 upstream | batch ads, ACL, hop count, streams |
| v4 node ↔ v3 upstream | falls back to Stage 2 per-domain control channels; no v4 features |
| v3 node/client ↔ v4 node | v3 path into the node; the node can mirror it upward with v4 |

Rollout:

1. Ship a v4-capable server that still serves v3. No behaviour change by default.
2. Add v4 to `htn`/`htc` behind `protocol = "auto"`. Mixed chains degrade per
   hop, never break.
3. Enable v4 by default once the session/stream tests are green.
4. Deprecate v3 only after a release with both paths.

## Implementation map

- `src/protocol.rs`: add the v4 types and encode/decode helpers; leave v3 intact.
- `src/transport/websocket.rs`: add a frame-oriented session bridge; keep the
  byte-stream bridge for v3.
- `src/server.rs`: `Session` registry, prefix routing, stream dispatch, ACL,
  route generations; keep v3 control/data channels.
- `src/client.rs`: a session runner that advertises the route set and opens
  streams to `local_addr`; v3 runner stays for fallback.
- `src/node.rs`: the mirror task becomes an advertiser (one session, batched
  diffs) when v4 is negotiated; the v3 mirror remains.
- `src/http.rs`: route `/_http-tunnel/v4/connect`, reserve the host, prefetch the
  header, open a stream instead of a data channel.
- `src/config.rs`: `protocol`, `[[nodes]]`, limits, `max_hops`, session windows.
- `docs/`, `examples/`, `Dockerfile`/release scripts: v4 docs and a mixed-chain
  example.

## Test plan

- Unit: frame encode/decode, prefix containment, ACL matching, hop validation,
  generation ordering.
- Session: handshake/auth (good token, wrong token, node/token mismatch, replayed
  nonce), route snapshot + diff + rejection, GoAway.
- Streams: interleaved streams, half-close, reset on connect failure, flow
  control (a slow reader pauses the sender), large bodies, WebSocket upgrades.
- Node: 3-hop chain with v4 sessions, fan-out, route withdrawal on leaf stop,
  ingress restart and re-advertisement, loop rejection, ACL rejection.
- Compatibility: v3/v4 matrix above; a chain with one v3 hop.
- Process E2E: real `hts`/`htn`/`htc` binaries, including a mixed v3/v4 chain.
- Soak: 1 000 streams, 1 000 routes, route churn.

## Open questions

1. **Binary schema stability.** bincode is compact but not self-describing. Is a
   versioned field list enough, or should v4 use a self-describing format
   (postcard/CBOR) for forward compatibility?
2. **Conflict policy.** Reject duplicates by default (recommended) or keep v3's
   last-writer-wins? This changes takeover semantics for multi-tenant relays.
3. **Head-of-line blocking.** Is one session per peer enough, or should sharding
   be part of v1 of the protocol?
4. **Hop enforcement.** Is advertisement-level loop detection sufficient, or
   must per-request hop counts be carried through the node's loopback listener
   (which would require either an internal header or an in-process handoff)?
5. **Warm pool.** v4 opens streams on demand; is a warm stream pool worth the
   extra state for latency-sensitive short requests?
6. **ACL source.** Static `[[nodes]]` only, or an external auth hook/resource?
7. **Timeout defaults.** Session idle, stream open timeout and drain deadline.
8. **Migration window.** How long must v3 coexist after v4 is enabled by default?

## Recommendation

Do not start v4 until the conflict policy, ACL source and HOL answer are agreed.
The smallest useful next step is a prototype that covers only one v4 session per
peer, exact/wildcard route advertisements, streams with flow control, and the
v3 fallback, leaving identity/ACL and hop counts behind a follow-up. That
prototype can be feature-gated and does not touch the v3 path.
