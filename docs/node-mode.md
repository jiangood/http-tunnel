# Node Mode

A *node* is one process per relay hop. It listens for visitors and downstream
peers, optionally dials one upstream, and can serve local `[[proxies]]`. It
replaces the two-process (`hts` + `htc`) workaround for chains of three or more
network segments where only one segment can reach the public ingress.

A node has two upstream modes:

- **Route mirroring (`mirror = true`, recommended).** Every domain published by
  a downstream client or a local proxy is claimed on the next hop, one control
  channel per domain. Each hop sees the real domain set, and a downstream that
  disconnects is withdrawn from the hop above.
- **One claim (`claim`).** The node claims one exact or wildcard domain on the
  next hop. It is coarser and cheaper, and keeps the domain names hidden above
  the relay, at the cost of routing unknown subdomains through the whole chain.

The wire protocol does not change in either mode: a node is a server and a client
in one process, so `hts`/`htc` instances of the same release can be mixed with
nodes in a chain.

## Topology

```
visitor nas.relay.example.com
  -> public hts (server_port 80, server_host control.example.com)
  -> htn A   (server_port 9000, server_host a.lan)   mirror = true
  -> htn B   (server_port 9001, server_host b.lan)   mirror = true
  -> htc C   (tunnel nas.relay.example.com -> 127.0.0.1:80)
```

With `claim` instead, A could claim `*.relay.example.com` and B could claim
`*.b.relay.example.com`, and the visitor would be
`nas.b.relay.example.com`. Both models can be mixed hop by hop.

Every hop dials the next one; only the ingress accepts connections from the
public Internet. A node without `[upstream]` is an ingress, the same role as
`hts`.

The data plane is recursive. The upstream tunnel of a node forwards each visitor
to `127.0.0.1:<server_port>` on that node, where the embedded server routes the
request by `Host` again. Nothing rewrites the `Host` header or the payload, so
WebSocket upgrades and any other HTTP upgrade pass through unchanged.

## Configuration

`htn` reads `htn.toml` from its working directory, and writes the example file
there on first start. The file is validated strictly, and `deny_unknown_fields`
rejects typos.

```toml
name = "relay-b"          # announced as the client name on the next hop
server_port = 9001        # required; there is no default and 0 is rejected
server_host = "b.lan"     # exact Host reserved for peer WebSocket connections
token = "downstream-secret"   # used by downstream clients and nodes

[upstream]
server = "ws://a.lan:9000"    # or wss://relay.example.com
token = "upstream-secret"     # used on the next hop

# One of the two modes:
mirror = true
# claim = "*.b.relay.example.com"

# Optional local services, served by this node itself.
[[proxies]]
name = "local"
domain = "local.b.relay.example.com"
local_addr = "127.0.0.1:8080"
```

| Setting | Meaning |
| --- | --- |
| `name` | The name sent in `ClientHello`; the next hop uses it in logs and in takeover warnings |
| `server_port` | The only listening port: visitors, downstream peers and the WebSocket endpoint share it. Required, and must not be `0` |
| `bind_host` | The IP address the entrypoint binds. Defaults to `0.0.0.0`; it must be `0.0.0.0` or a loopback address, because the upstream tunnel forwards visitors to `127.0.0.1:<server_port>` |
| `server_host` | The exact `Host` reserved for the WebSocket control/data channels. It must not be a wildcard |
| `token` | Shared secret of the downstream clients and nodes |
| `[[proxies]]` | Local services served by this node. Same fields as on `hts`: `name`, `domain`, `local_addr`, optional `auth_header` |
| `[upstream].server` | WebSocket endpoint of the next hop. `wss` defaults to port 443, `ws` to 80 |
| `[upstream].token` | Shared secret of the next hop |
| `[upstream].mirror` | Mirror every route of the embedded server to the next hop, one control channel per domain |
| `[upstream].claim` | Instead of mirroring, claim one exact or wildcard domain on the next hop |
| `heartbeat_timeout` | Application heartbeat timeout of the upstream control channels, 40 s by default; `0` disables it |
| `retry_interval` | Upper bound of the reconnect backoff to the next hop, 1 s by default |
| `max_mirrored_routes` | The maximum number of routes mirrored to the next hop, 256 by default; `0` disables the cap. Raise it together with the next hop's `max_control_connections` |
| `keepalive_interval` | WebSocket ping interval for the embedded server and the upstream tunnels, 15 s by default; `0` disables it |
| limits | `max_pending_handshakes`, `max_control_connections`, `max_data_connections`, `pool_min`, `pool_max`, with the same defaults as `hts` |

### Route mirroring

The embedded server publishes a set of routable domains: every exact or wildcard
domain claimed by a downstream client, plus the local `[[proxies]]`, minus the
reserved `server_host`. A node with `mirror = true` subscribes to that set and
keeps one upstream control channel per domain, always with
`local_addr = 127.0.0.1:<server_port>`.

- A route that appears downstream is claimed on the next hop.
- A route that disappears is withdrawn, after its in-flight visitors finish.
- Wildcard routes published downstream are mirrored as wildcards.
- A route that overlaps the next hop's `server_host` is skipped with a warning:
  that host is reserved for the control endpoint and can never be registered as
  a business route.
- Local proxies are mirrored too, so a visitor is routed to the node and served
  there, or relayed further down.
- No wire-protocol change is involved: each upstream control channel still claims
  exactly one domain.

The node logs each route it starts and stops mirroring. The upstream never sees
the routes of the node before the node has learned them, so a visitor that
arrives while a downstream is still connecting is answered with `404` at the
upper hop instead of being forwarded blindly.

### Validation rules

Startup fails when:

- `server_port` is `0`.
- `bind_host` is not an IP address, or is neither `0.0.0.0` nor a loopback
  address (the upstream tunnel forwards visitors to `127.0.0.1`).
- `name` is empty.
- `token` or `[upstream].token` is empty, or is still the placeholder of the
  generated template.
- `server_host` is a wildcard or not a valid host.
- A `[[proxies]]` entry is invalid or overlaps `server_host`.
- `[upstream].server` is not a `ws://` or `wss://` URL.
- Neither `claim` nor `mirror = true` is set, or both are set.
- `claim` is not a valid domain, or overlaps this node's `server_host` or the
  `server_host` of the next hop (the host of `[upstream].server`).
- `[upstream].server` points at the node's own listener: the same hostname and
  port, or a loopback/unspecified address on the node's own port. At startup the
  name is resolved, so a DNS alias that resolves to loopback is rejected too.

A local `[[proxies]]` entry **may** sit inside the claimed subtree: the node
serves those domains itself and relays the rest to its downstream peers.
Proxies take precedence over downstream tunnels, as on a server. In mirror mode,
the proxy domains are published to the next hop like any other route.

## Lifecycle

1. Parse and validate `htn.toml`.
2. Bind `bind_host:server_port`; a port that cannot be bound fails startup.
3. Start the embedded server with `server_host`, `token`, `[[proxies]]` and the
   limits.
4. If `[upstream]` is present, start the upstream mode: one tunnel for `claim`,
   or a mirror task that follows the route set and forwards visitors back to
   `127.0.0.1:<server_port>`.
5. On shutdown: stop accepting, withdraw every upstream route, drain the
   in-flight visitors (up to 30 s), then drop the downstream control channels.

Withdrawing the upstream routes first matters: it stops new visitors from being
routed to the node while the embedded server drains the ones already in flight.
Closing an upstream control channel does not cut those visitors off; their data
channels keep forwarding until they finish.

A node without `[upstream]` behaves like `hts`: it drains visitors and then drops
the downstream control channels.

## Failure handling

- When a downstream client or node disconnects, its route is removed from the
  node's routing table and, in mirror mode, withdrawn from the hop above.
  Visitors for that domain are answered with `404` (no route at the upper hop) or
  `503`/`504` (the tunnel is gone or did not provide a data channel) instead of
  hanging.
- A silently dead peer is detected by the WebSocket keepalive after three
  `keepalive_interval` periods (45 s by default). Until then, the hop above may
  still send visitors, which are answered with an error by this node.
- Upstream tunnels reconnect with an exponential backoff and re-claim their
  routes automatically. A rejected token is fatal: `htn` reports it and exits
  instead of retrying forever.
- A route rejected by the upstream for a permanent reason (for example it
  overlaps the upstream's reserved `server_host`) is skipped in mirror mode and
  rejected at startup for `claim`. Other rejections are retried like any other
  failure; check the upstream logs when a mirrored domain never becomes
  reachable.

## Deployment notes

- **TLS is hop by hop.** A `wss://` upstream only protects that hop. There is no
  end-to-end encryption between hops; use a VPN overlay if the hops themselves
  are untrusted.
- **Use a different token per hop.** A shared tree-wide token lets any leaf claim
  any domain at any reachable hop and collapses the trust boundaries the chain
  is supposed to create.
- **One port per node.** `server_host` reserves the peer endpoint on the same
  port as the visitors. Do not put a `[[proxies]]` or downstream tunnel on it.
- **Mirroring costs one connection per route.** Each mirrored domain keeps one
  control channel plus the warm data channels of the hop above (`pool_min`, 4 by
  default). On a relay with many domains, set `pool_min = 1` on the relay that
  accepts the mirrored control channels, and raise `max_control_connections`
  deliberately: the default of 256 caps the number of mirrored domains per hop.
  `max_data_connections` (2048 by default) caps the warm pools; at `pool_min = 4`
  that is roughly 500 domains before the pools starve. A node stops mirroring
  beyond `max_mirrored_routes` (256 by default, `0` means unlimited) and logs a
  warning, so a growing route set does not exhaust the next hop one failed
  control channel at a time. The first domains in lexicographic order are
  mirrored when the cap is reached; raise the cap and the next hop's
  `max_control_connections` together.
- **Claim mode is coarser and cheaper.** With `claim = "*.example.com"` the upper
  hop sees one domain, keeps one tunnel, and cannot tell which downstream
  domains exist. Use it when the namespace is hierarchical and the upper hops
  should not see the real names.
- **The public ingress must be a `server` or a node without `[upstream]`.** A
  node that dials itself is rejected at startup, and a node without
  `[upstream]` is not exposed unless its `server_port` is reachable.
- **Domain takeovers still follow the v3 rule.** Any token holder may claim a
  domain that another client serves; the newer control channel takes it over and
  the server logs a warning. Do not enable mirroring for untrusted peers until
  per-node identities and ACLs exist.

## Mixing with `hts` and `htc`

Protocol v3 is unchanged, so a chain can be built from any mix of `hts`, `htc`
and `htn` binaries **of the same release**. Protocol versions must match exactly
across the chain; upgrading one hop without the others is not supported.

## Example: three hops with mirroring

`hts.toml` on the public server:

```toml
server_port = 443
server_host = "control.example.com"
token = "ingress-secret"
```

`htn.toml` on relay A:

```toml
name = "relay-a"
server_port = 9000
server_host = "a.lan"
token = "a-downstream-secret"

[upstream]
server = "wss://control.example.com"
token = "ingress-secret"
mirror = true
```

`htn.toml` on relay B:

```toml
name = "relay-b"
server_port = 9001
server_host = "b.lan"
token = "b-downstream-secret"

[upstream]
server = "ws://a.lan:9000"
token = "a-downstream-secret"
mirror = true
```

`htc.toml` on the service host behind relay B:

```toml
server = "ws://b.lan:9001"
name = "nas"
token = "b-downstream-secret"

[[tunnels]]
name = "nas"
domain = "nas.relay.example.com"
local_addr = "127.0.0.1:80"
```

The leaf claims `nas.relay.example.com` on relay B. Relay B mirrors it to relay
A, which mirrors it to the ingress; a visitor of
`https://nas.relay.example.com` is routed directly at every hop. Stopping the
leaf removes the route from B, A and the ingress in turn.

To hide the domains instead, replace `mirror = true` with
`claim = "*.relay.example.com"` on A and `claim = "*.b.relay.example.com"` on B,
and use `nas.b.relay.example.com` as the leaf domain.
