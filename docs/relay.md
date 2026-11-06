# Multi-hop relay mode

`ht` is one universal process. It can listen (`[server]`) and/or dial one
upstream (`[upstream]`). A relay chain is therefore one process per hop, with the
same `ht.toml` schema everywhere.

```text
visitor
  -> ingress ht       [server], no [upstream]
  -> relay A ht       [server] + [upstream] -> ingress
  -> relay B ht       [server] + [upstream] -> relay A
  -> leaf ht          [upstream] -> relay B, local [[proxies]]
  -> local service
```

The roles are selected by the two tables:

- **standalone ingress**: `[server]`, no `[upstream]`; listens for visitors and
  downstream peers.
- **relay**: both tables; listens for its downstream peers and also opens one v4
  session to its next hop.
- **leaf**: `[upstream]`, no `[server]`; never binds a port and sends visitor
  streams straight to its local `[[proxies]]`.

## Configuration

```toml
name = "relay-b"

[server]
port = 9001
bind = "0.0.0.0"
domain = "b.lan"
token = "downstream-secret"

[[proxies]]
name = "local"
domain = "local.b.relay.example.com"
local_addr = "127.0.0.1:8080"

[upstream]
url = "ws://a.lan:7777"
token = "upstream-secret"
allow_modify = false
retry_interval = 1
keepalive_interval = 15
```

| Setting | Meaning |
| --- | --- |
| `[server]` | Server-mode switch. Omit it to never bind a port. |
| `[server].port` | The listener port for visitors, downstream peers, and the WebSocket endpoint. Defaults to `7777`; `0` picks an ephemeral port and requires `[upstream]`. |
| `[server].bind` | The listener address, an IPv4/IPv6 literal. Defaults to `127.0.0.1` when omitted; the generated template writes `0.0.0.0` so Docker, a LAN relay, or direct public access works out of the box. `0.0.0.0` exposes the port to every reachable network, so keep the token strong and restrict the source with a firewall when it is public. |
| `[server].domain` | Exact Host reserved for the v4 session and management endpoint. Omit it to serve visitors only, with no control plane. |
| `[server].token` | Token for downstream peers. May be omitted with `[upstream]`, in which case the upstream token is reused. |
| `[server].max_pending_handshakes` | Maximum concurrently unauthenticated upgrades. Defaults to `256`. |
| `[server].max_sessions` | Maximum concurrent v4 sessions. Defaults to `256`. |
| `[server].keepalive_interval` | WebSocket ping interval of the listener, in secs. Defaults to `15`; `0` disables keepalive. |
| `[[proxies]]` | Local services served by this process. They take precedence over downstream session routes. |
| `[upstream].allow_modify` | Whether the next hop may change the local `[[proxies]]` over v4. Defaults to `false`; the next hop may still list them. The node's own page can switch it at runtime. |
| `[upstream].retry_interval` | Interval between reconnects to the next hop, in secs. Defaults to `1`. |
| `[upstream].keepalive_interval` | WebSocket ping interval of the upstream session, in secs. Defaults to `15`; `0` disables keepalive. |
| `[upstream]` | The next hop. Remove it to run as a standalone ingress. |

## Route advertisement

- With `[server]`, the route watch becomes a set of routes that all point back to
  its own listener: `127.0.0.1:<actual port>` for a wildcard IPv4 bind,
  `[::1]:<actual port>` for a wildcard IPv6 bind, or the configured `bind` when
  it is a specific address. The set is the union of local `[[proxies]]` and the
  routes claimed by downstream v4 sessions.
- Without `[server]`, each local `[[proxies]]` is its own target, so the v4
  stream connects directly to `local_addr`. A listener-less node cannot have
  downstream sessions.
- Additions and removals are sent as v4 `Update` frames on the same session.
- There is no switch: a process with `[upstream]` always mirrors its route set.
  The old `mirror`, `claim` and `max_mirrored_routes` keys are rejected with a
  migration hint, so an upgrade cannot silently change the advertised topology.
- Duplicate domains are rejected as `Conflict`. A rejected route is logged and
  does not stop the process.
- A relay also advertises `CAP_PROXY_ADMIN` for its own local services and
  announces `CAP_PROXY_WRITE` only when `[upstream].allow_modify` is on.
  Toggling the switch reconnects the upstream session so the new capability is
  announced immediately.

## Data plane

With `[server]`, the data plane is recursive. The upstream session receives a
visitor stream and connects to `127.0.0.1:<actual port>`. The embedded listener
reads the `Host` again and routes the visitor to a local proxy or a downstream
session.

Without `[server]`, the upstream session connects directly to the route's
`local_addr`. The `Host` header and the bytes are not rewritten in either case,
so WebSocket upgrades and any other protocol upgrade traverse the whole chain.

With a listener, one private header is added on the loopback hop:
`x-http-tunnel-request-id` carries the visitor's id from the session into the
embedded listener, which adopts and strips it. The id therefore stays the same
from the ingress through every relay to the leaf, in every log line and in the
error page. Only a loopback peer, or a peer using the specific address the
listener is bound to, may use it; an id from any other peer is ignored and
stripped.

Each hop uses one v4 session for every domain and every visitor stream.
WebSocket ping/pong provides keepalive.

## Validation and safety

Startup validates before advertising anything:

- `[server]` is required without `[upstream]`.
- `[server].port = 0` is rejected without `[upstream]`.
- `[server].domain` must be an exact hostname and is not allowed without
  `[server]`.
- Local proxy names and domains are unique after lowercasing; wildcard domains
  must not overlap `[server].domain`.
- `[upstream].url` must be a `ws://` or `wss://` URL without a path.
- The upstream token must be a strong non-placeholder secret.
- The upstream must not point at this process's own listener. The literal
  reserved host, loopback/unspecified addresses, and same-port endpoints are
  rejected at configuration time; a DNS name resolving back to the local machine
  is rejected after the listener is bound, using the actual ephemeral port.
- A warning is logged when the listener token and the upstream token are the
  same; use a different token per hop.

## Shutdown

On shutdown the process:

1. stops the upstream session first, which withdraws its advertised routes;
2. if `[server]` is present, drains in-flight visitors for up to 30 seconds;
3. stops downstream v4 sessions and closes the listener.

A fatal authentication failure on the upstream (wrong token) is reported and
ends the process instead of retrying forever. Other network failures reconnect
with backoff, re-advertising the current route set on the new session.
