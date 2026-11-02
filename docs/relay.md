# Multi-hop relay mode

`ht` is one universal process. It always runs an embedded server and can
optionally dial one upstream. A relay chain is therefore one process per hop,
with the same `ht.toml` schema everywhere.

```text
visitor
  -> ingress ht       no [upstream], serves visitors directly
  -> relay A ht       [upstream] -> ingress, mirrors routes
  -> relay B ht       [upstream] -> relay A, mirrors routes
  -> leaf ht          [upstream] -> relay B, local [[proxies]]
  -> local service
```

The role is selected only by the presence of `[upstream]`:

- **standalone ingress**: no `[upstream]`; listens for visitors and downstream
  peers.
- **relay / leaf**: has `[upstream]`; also opens one v4 session to its next hop
  and advertises routes.

## Configuration

```toml
name = "relay-b"
listen_port = 9001
control_host = "b.lan"
token = "downstream-secret"

[[proxies]]
name = "local"
domain = "local.b.relay.example.com"
local_addr = "127.0.0.1:8080"

[upstream]
url = "ws://a.lan:7777"
token = "upstream-secret"
```

| Setting | Meaning |
| --- | --- |
| `listen_port` | The one listener port for visitors, downstream peers, and the WebSocket endpoint. `0` picks an ephemeral port and requires `[upstream]`. |
| `control_host` | Exact Host reserved for the v4 session endpoint. Required without `[upstream]`; defaults to `127.0.0.1` with it. |
| `token` | Token for downstream peers. May be omitted with `[upstream]`, in which case the upstream token is reused. |
| `[[proxies]]` | Local services served by this process. They take precedence over downstream session routes. |
| `allow_upstream_modify` | Whether the next hop may change the local `[[proxies]]` over v4. Defaults to `false`; the next hop may still list them. The node's own page can switch it at runtime. |
| `[upstream]` | The next hop. Remove it to run as a standalone ingress. |

## Route advertisement

- The embedded server's route watch becomes a set of routes that all point back
  to `127.0.0.1:<actual listen_port>`. The set is the union of local
  `[[proxies]]` and the routes claimed by downstream v4 sessions. Additions and
  removals are sent as v4 `Update` frames on the same session.
- There is no switch: a process with `[upstream]` always mirrors its route set.
  The old `mirror`, `claim` and `max_mirrored_routes` keys are rejected with a
  migration hint, so an upgrade cannot silently change the advertised topology.
- Duplicate domains are rejected as `Conflict`. A rejected route is logged and
  does not stop the process.
- A relay also advertises `CAP_PROXY_ADMIN` for its own local services and
  announces `CAP_PROXY_WRITE` only when `allow_upstream_modify` is on and
  `HT_PROXIES` is not managing the list. Toggling the switch reconnects the
  upstream session so the new capability is announced immediately.

## Data plane

The data plane is recursive. The upstream session receives a visitor stream and
connects to `127.0.0.1:<actual listen_port>`. The embedded listener reads the
`Host` again and routes the visitor to a local proxy or a downstream session.
The `Host` header and the bytes are not rewritten, so WebSocket upgrades and any
other protocol upgrade traverse the whole chain.

One private header is added on that loopback hop:
`x-http-tunnel-request-id` carries the visitor's id from the session into the
embedded listener, which adopts and strips it. The id therefore stays the same
from the ingress through every relay to the leaf, in every log line and in the
error page. Only a loopback peer may use it; an id from any other peer is
ignored and stripped.

Each hop uses one v4 session for every domain and every visitor stream.
WebSocket ping/pong provides keepalive.

## Validation and safety

Startup validates before advertising anything:

- `listen_port = 0` is rejected without `[upstream]`.
- `control_host` is required without an upstream and must be an exact hostname.
- Local proxy names and domains are unique after lowercasing; wildcard domains
  must not overlap `control_host`.
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
2. drains in-flight visitors for up to 30 seconds;
3. stops downstream v4 sessions and closes the listener.

A fatal authentication failure on the upstream (wrong token) is reported and
ends the process instead of retrying forever. Other network failures reconnect
with backoff, re-advertising the current route set on the new session.
