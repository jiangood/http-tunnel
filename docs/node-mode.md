# Multi-hop relay mode

`ht` is one universal process. It always runs an embedded server and can
optionally dial one upstream. A relay chain is therefore one process per hop,
with the same `ht.toml` schema everywhere.

```text
visitor
  -> ingress ht       no [upstream], serves visitors directly
  -> relay A ht       [upstream] -> ingress, mirror or claim
  -> relay B ht       [upstream] -> relay A, mirror or claim
  -> leaf ht          [upstream] -> relay B, local [[proxies]]
  -> local service
```

There is no mode flag and no separate binary. The role is selected only by the
presence of `[upstream]`:

- **standalone ingress**: no `[upstream]`; listens for visitors and downstream
  peers.
- **relay / leaf**: has `[upstream]`; also opens one v4 session to its next hop
  and advertises routes.

## Configuration

```toml
name = "relay-b"
server_port = 9001
bind_host = "0.0.0.0"
server_host = "b.lan"
token = "downstream-secret"

[[proxies]]
name = "local"
domain = "local.b.relay.example.com"
local_addr = "127.0.0.1:8080"

[upstream]
server = "ws://a.lan:7777"
token = "upstream-secret"
mirror = true
# claim = "*.b.relay.example.com"
```

| Setting | Meaning |
| --- | --- |
| `server_port` | The one listener port for visitors, downstream peers, and the WebSocket endpoint. `0` picks an ephemeral port and requires `[upstream]`. |
| `bind_host` | IP the listener binds. Defaults to `0.0.0.0`; with `[upstream]` it must be `0.0.0.0` or a loopback address, because visitors arrive from the upstream session on `127.0.0.1:<actual port>`. |
| `server_host` | Exact Host reserved for the WebSocket control endpoint. Required without `[upstream]`; defaults to `127.0.0.1` with it. |
| `token` | Token for downstream peers. May be omitted with `[upstream]`, in which case the upstream token is reused. |
| `[[proxies]]` | Local services served by this process. They take precedence over downstream session routes. |
| `[upstream]` | The next hop. Remove it to run as a standalone ingress. |
| `mirror` | Publish the local proxies and downstream routes on the next hop. Default when neither `mirror` nor `claim` is set. |
| `claim` | Publish one exact or wildcard domain instead. Mutually exclusive with `mirror = true`. |
| `max_mirrored_routes` | Cap on mirrored routes; `0` disables the cap. Default `256`. |

## Route advertisement

- `mirror = true` turns the embedded server's route watch into a set of routes
  that all point back to `127.0.0.1:<actual server_port>`. The set is the union
  of local `[[proxies]]` and the routes claimed by downstream v4 sessions.
  Additions and removals are sent as v4 `Update` frames on the same session.
- `claim = "..."` advertises exactly one exact or wildcard domain. The upper
  hops then see only that domain.
- The claim must not overlap this process's `server_host` or the next hop's
  `server_host` (the host of the upstream URL).
- A local proxy inside a claimed subtree is intentional: the relay serves that
  domain itself and forwards the rest downstream.
- Duplicate domains are rejected as `Conflict`. A rejected claim is logged and
  does not stop the process.

## Data plane

The data plane is recursive. The upstream session receives a visitor stream and
connects to `127.0.0.1:<actual server_port>`. The embedded listener reads the
`Host` again and routes the visitor to a local proxy or a downstream session.
The `Host` header and the bytes are not rewritten, so WebSocket upgrades and any
other protocol upgrade traverse the whole chain.

Each hop uses one v4 session for every domain and every visitor stream. There is
no per-domain control channel, no connection warm-up pool, and no separate
application-layer heartbeat; WebSocket ping/pong provides keepalive.

## Validation and safety

Startup validates before advertising anything:

- `server_port = 0` is rejected without `[upstream]`.
- With `[upstream]`, `bind_host` must be unspecified or loopback.
- `server_host` is required without an upstream and must be an exact hostname.
- Local proxy names and domains are unique after lowercasing; wildcard domains
  must not overlap `server_host`.
- `[upstream].server` must be a `ws://` or `wss://` URL without a path.
- The upstream token must be a strong non-placeholder secret.
- `mirror` and `claim` are mutually exclusive; `mirror` defaults on.
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
