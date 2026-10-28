# Internals

## Concepts

### Local service / proxy
The entry in `[[proxies]]` that maps a domain to a local TCP address or Unix
Socket. The same entry is used whether the process is an ingress, a relay or a
leaf. It optionally carries an `auth_header` gate.

### Listener / embedded server
Every `ht` process runs an embedded HTTP server. It listens on `listen_port`,
serves local proxies, routes visitors to downstream v4 sessions, and accepts v4
sessions on the reserved `control_host`.

### Upstream
The optional `[upstream]` table. When present, the process opens one v4 session
to the next hop and advertises either its full route set (`mirror`) or one
`claim`. When absent, the process is a standalone ingress and no session is
opened.

### Route
A domain served by the process. The route watch is the union of local
`[[proxies]]` and the prefixes claimed by downstream v4 sessions, excluding the
reserved `control_host`.

### Visitor
Who visits a route through the public ingress.

### Session
One WebSocket between two processes, carrying v4 frames. It announces the peer
identity, advertises routes as a snapshot and diffs, and multiplexes every
visitor stream. One downstream process has one session.

### Stream
A logical byte pipe inside a session, one per visitor. The upstream side opens a
stream with the resolved domain and the visitor's prefetched HTTP header; the
downstream connects to the route's `local_addr` and pipes bytes. Streams have
half-close, a reset reason, and a per-direction credit window.

## The process

`ht` reads `ht.toml`, then overlays `HT_*` environment variables. Configuration
validation normalizes domains,
requires strong tokens, rejects duplicate routes, and rejects an upstream that
points at the process itself.

The process binds the listener before doing anything else. That guarantees the
route is never advertised before the process can serve a visitor, and it resolves
an ephemeral `listen_port = 0` to the actual port used as the loopback target of
the upstream session.

The embedded server accepts visitors on one port. It reads the request line and
`Host`, resolves a local proxy first and then a downstream session, and either
connects to the local service or opens a stream. The visitor's prefetched header
is sent in `OpenStream`; the downstream applies the route's `auth_header`, writes
the header and pipes the rest. The response comes back over the same stream.

The proxy list is held behind an `RwLock` in `ServerState`, so the management API
can replace it at runtime. The route watch is recomputed from the local proxies
and the downstream session routes; `http.rs` reads the current proxy list on each
visitor, and a mirroring session sees the new route immediately.

A stream whose downstream cannot reach its `local_addr` is reset with
`ConnectFailed`; the server answers the visitor with `502`. A stream whose
session dies is closed and the visitor gets a `503`/`504` instead of hanging. A
duplicate advertisement from another session is rejected with `Conflict`.

Each WebSocket bridge sends a protocol ping every `keepalive_interval` seconds
(15 by default) and gives up after three intervals without any inbound frame, so
a silently dropped session is detected within 45 seconds. v4 relies on that
keepalive; there is no separate application heartbeat.

## Upstream session

With `[upstream]`, the process converts its route watch into a `RouteMap` whose
targets all point to `127.0.0.1:<actual listen_port>`. This is true for both
`mirror` and `claim`; the difference is only what is advertised:

- `mirror`: every route of the union above, sent as a snapshot and then diffed
  as `Update` frames.
- `claim`: one exact or wildcard prefix.

When the upstream session receives an `OpenStream`, it connects to the loopback
target. The embedded listener routes the request by `Host` again. The data plane
is recursive and the HTTP request is never rewritten, so WebSocket upgrades and
other upgrades traverse any number of hops.

## Shutdown

On shutdown the process:

1. sends `GoAway` on the upstream session, withdrawing its routes;
2. stops the listener and waits for in-flight visitors for up to 30 seconds;
3. closes downstream v4 sessions.

A fatal upstream authentication failure (wrong token) ends the process instead
of retrying. Transient network failures reconnect with backoff and re-advertise
the current route set on the new session.

## Protocol v4

The frames, route advertisement, stream flow control and identity are described
in [protocol-v4.md](./protocol-v4.md). v4 is the only protocol; every hop must
run a v4-compatible `ht`.
