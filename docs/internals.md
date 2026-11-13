# Internals

## Concepts

### Local service / proxy
The entry in `[[proxies]]` that maps a domain to a local TCP address or Unix
Socket. The same entry is used whether the process is an ingress, a relay or a
leaf. It optionally carries an `x-auth` gate.

### Listener / embedded server
The optional `[server]` table. When present, the process runs an embedded HTTP
server on `[server].port`, serves local proxies, routes visitors to downstream v4
sessions, and accepts v4 sessions on the reserved `[server].domain`. When the
table is absent, the process never binds a port and is a pure client/leaf.

### Upstream
The optional `[upstream]` table. When present, the process opens one v4 session
to the next hop and mirrors its route set there. A process with `[server]` and no
`[upstream]` is a standalone ingress; a process with `[upstream]` and no
`[server]` is a leaf; both tables make it a relay.

### Route
A domain served by the process. The route watch is the union of local
`[[proxies]]` and the prefixes claimed by downstream v4 sessions, excluding the
reserved `[server].domain` when one is configured.

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

`ht` reads `ht.toml`; it is the only configuration source. Configuration
validation normalizes domains,
requires strong tokens, rejects duplicate routes, and rejects an upstream that
points at the process itself.

While it runs, the process watches its configuration file (polling its content
about once a second, which also coalesces the several writes of an editor save).
A change to `[[proxies]]` or `[upstream].allow_modify` is applied to the running
server and session, exactly like the management API. Any other change stops the
current generation through the normal shutdown sequence below and starts a new
one. The listener socket is owned by the supervisor, not by a generation: when
`[server].port` and `[server].bind` do not change it stays bound across the
restart, so new visitors queue instead of getting a refused connection; when the
port changes the new listener is bound before the old generation is stopped, and a
bind failure leaves the running configuration untouched. A same-port `bind`
change is the exception: the overlapping old socket must be released before the
new address can be bound, so the supervisor stops the generation first and
restores the previous listener if the new address is unusable. Adding or removing
the whole `[server]` table uses the same path. Routes are advertised again, and the
process keeps running. A management API write is marked before it hits the file,
so the watcher does not tear the generation down for a change that already took
effect live. A file that is missing, half-written or invalid is logged and
ignored; the last valid configuration keeps serving. The watcher and the
supervisor share a `ReloadStatus`, so the management page and `/api/status` can
show the last reload error and when the last successful apply happened. The
management page edits the same file through `PUT /api/settings`, so this is the
only configuration path. Saving merges into the existing `toml_edit` document,
which preserves comments and unchanged entries. The page
asks before saving a change that needs a rebuild, and `POST /api/restart` (or
`PUT /api/settings?restart=true`) asks the supervisor for the same rebuild
immediately, even when the configuration equals the running one. The process
itself never exits for either path.

When `[server]` is present, the process binds the listener before doing anything
else. That guarantees the route is never advertised before the process can serve
a visitor, and it resolves an ephemeral `[server].port = 0` to the actual port
used as the loopback target of the upstream session. `[server].bind` selects the
interface, defaulting to `127.0.0.1`; a specific address is used as the relay's
local target, while the wildcard binds map to `127.0.0.1` or `::1`. Without
`[server]`, startup binds nothing and the session advertises direct proxy targets.

The embedded server accepts visitors on one port. It reads the request line and
`Host`, resolves a local proxy first and then a downstream session, and either
connects to the local service or opens a stream. The visitor's prefetched header
is sent in `OpenStream`; the downstream applies the route's `x-auth`, writes
the header and pipes the rest. The response comes back over the same stream.

The proxy list is held behind an `RwLock` in `ServerState`, so the management API
can replace it at runtime. The route watch is recomputed from the local proxies
and the downstream session routes; `http.rs` reads the current proxy list on each
visitor, and a mirroring session sees the new route immediately.

A stream whose downstream cannot reach its `local_addr` is reset with
`ConnectFailed`; the server answers the visitor with `502`. A stream whose
session dies is closed and the visitor gets a `503`/`504` instead of hanging. A
duplicate advertisement from another session is rejected with `Conflict`,
unless the advertising session announces the same node id as the holder, in
which case it takes the route over (a restarted node reclaims its own routes).
The advertiser retries a `Conflict` on a slow timer, so a stale holder only
delays the route instead of leaving it down.

Each WebSocket bridge sends a protocol ping every `[server].keepalive_interval`
or `[upstream].keepalive_interval` seconds (15 by default) and gives up after
three intervals without any inbound frame, so
a silently dropped session is detected within 45 seconds. v4 relies on that
keepalive; there is no separate application heartbeat.

## Upstream session

With `[upstream]`, the process converts its route watch into a `RouteMap`. When
`[server]` is present, every target points to its own listener:
`127.0.0.1:<actual port>` for a wildcard IPv4 bind, `[::1]:<actual port>` for a
wildcard IPv6 bind, or the configured `bind` when it is a specific address. When
`[server]` is absent, each local `[[proxies]]` is its own target and the stream
goes straight to `local_addr`; downstream route prefixes cannot exist without a
listener. Every route of the union above is advertised: a snapshot first, then
diffs as `Update` frames. There is no per-process switch and no cap.

When the upstream session receives an `OpenStream`, it connects to the target.
With a listener, the embedded server routes the request by `Host` again; without
one, the target is the local service itself. The HTTP request is never rewritten
in either case, so WebSocket upgrades and other upgrades traverse any number of
hops.

## Shutdown

On shutdown the process:

1. sends `GoAway` on the upstream session, withdrawing its routes;
2. if `[server]` is present, stops the listener and waits for in-flight visitors
   for up to 30 seconds;
3. closes downstream v4 sessions.

A fatal upstream authentication failure (wrong token) ends the process instead
of retrying. Transient network failures reconnect with backoff and re-advertise
the current route set on the new session.

## Protocol v4

The frames, route advertisement, stream flow control and identity are described
in [protocol-v4.md](./protocol-v4.md). v4 is the only protocol; every hop must
run a v4-compatible `ht`.
