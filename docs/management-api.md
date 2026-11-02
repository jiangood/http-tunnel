# Web management and administration API

The universal `ht` process exposes a small web page and a REST API on the
same port. It manages the local `[[proxies]]` of the running process:

- A standalone ingress serves them directly.
- A leaf or relay also mirrors them to its `[upstream]`.

The management interface is always enabled and shares the reserved
`control_host` of the main `listen_port`; it has no listener or port of its own.

```toml
listen_port = 7777
control_host = "edge.lan"
```

Open `http://<control_host>:<listen_port>/`. Requests whose `Host` is not
`control_host` keep following the normal proxy/v4 routing, so a business domain
never reaches the management router.

The page itself is protected by the browser-native HTTP Basic login: username
`ht`, password = the listener `token`. It is the only Basic-authenticated
resource. It is a single HTML document with an inline stylesheet and script and
no external dependency; it renders nothing on the server and reads all of its
data from `/api` with the Bearer token it is handed after login. One node
card lists the local process together with every connected downstream v4
session. Every row offers "管理服务", which opens a dialog that lists, adds,
edits and deletes that node's own `[[proxies]]`; downstream rows also offer
"断开", which closes that one session. The route table is a read-only view of
the effective routes of both. The `HT_*` variables that are set are shown as
well.

## API

Every API call uses a Bearer token, which is also the listener `token`:

| Method | Path | Description |
| --- | --- | --- |
| GET | `/api/status` | Name, transport, upstream, connectivity, proxy count, session count, `allow_upstream_modify` and whether it is editable, version, config path, uptime, the `HT_*` variables that are set |
| GET | `/api/proxies` | List local `[[proxies]]` as `name`/`domain`/`local_addr`/`auth_header`/`source`; with `?node=` or `?session=`, list the proxies of that downstream node |
| GET | `/api/routes` | The effective routing table, including v4 owners |
| GET | `/api/sessions` | The connected downstream v4 sessions: session id, node, version, peer address, forwarded client address, role, uptime, owned domains, `manageable`, `writable` and `proxy_source` |
| PUT | `/api/proxies/{name}` | Create or replace a local proxy, including its `auth_header`; with `?node=`/`?session=`, write it on that downstream node |
| PUT | `/api/settings/allow-upstream-modify` | Allow or forbid the next hop from changing this process's local proxies; body `{"allowed":true}` |
| DELETE | `/api/proxies/{name}` | Delete a local proxy; with `?node=`/`?session=`, delete it on that downstream node |
| DELETE | `/api/sessions/{id}` | Close one downstream session |

`GET /api/proxies` returns the visitor token in `auth_header`; the field is
`null` when authentication is off:

```json
[
  {"name":"app","domain":"app.example.com","local_addr":"127.0.0.1:3000","auth_header":"visitor-secret","source":"file"},
  {"name":"nas","domain":"nas.example.com","local_addr":"127.0.0.1:80","auth_header":null,"source":"file"}
]
```

`GET /api/routes` shows the table the visitor path actually uses. Local routes
come first, then the downstream routes, each group sorted by domain. A local
service and a downstream session can claim the same domain; the local service
wins, but both entries are listed:

```json
[
  {"domain":"app.example.com","owner":"local","name":"app","target":"127.0.0.1:3000"},
  {"domain":"svc.example.com","owner":"v4","session_id":3,"node":"home"}
]
```

`GET /api/sessions` lists the downstream nodes that are connected right now. One
entry is one v4 session, so a node that reconnects appears with a new session id.
`peer` is the TCP address the session connected from. When a gateway (SafeLine,
nginx, a cloud load balancer) sits in front of the server, that is the gateway;
`forwarded` is then the address the gateway reports for the node, taken from the
last hop of `X-Forwarded-For` (or `X-Real-IP`, or `Forwarded`). When several
nodes announce the same or a wrong `node` id, `forwarded`/`peer` is what tells
them apart. The header can be spoofed by a peer that reaches the endpoint
directly, so it is diagnostic, not an authorization decision.

`version` is the software version the node announced in its WebSocket upgrade.
It is also diagnostic: an older node that does not send it omits the field, and
the page shows `—`.

```json
[
  {"session_id":3,"node":"home","version":"2.5.3","peer":"10.0.0.6:60216","forwarded":"203.0.113.9","role":"node","connected_seconds":125,"domains":["svc.example.com"],"manageable":true,"writable":false,"proxy_source":"file"}
]
```

`forwarded` is omitted when the node connects directly or the gateway does not
add a forwarding header; `version` is omitted by older nodes that do not
announce it. The page shows `forwarded` next to the TCP peer, labelled "经".

The page shows the same list under "下游节点", with the version and peer address
next to each node and the connected count next to the route count. A client that
only connects to an upstream does not serve sessions itself, so its own session
list is empty; use the entry node's page to see all of its downstream nodes.

The same route table is logged at `info` whenever it changes:

```
INFO http_tunnel::server: The route table changed routes=2
     table=["app.example.com -> local:app (127.0.0.1:3000)",
            "svc.example.com -> v4:3 (home)"]
```

```sh
curl -H "Host: edge.lan" -H "Authorization: Bearer $TOKEN" \
  http://127.0.0.1:7777/api/proxies
curl -X PUT -H "Host: edge.lan" -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"domain":"nas.example.com","local_addr":"127.0.0.1:80","auth_header":"visitor-secret"}' \
  http://127.0.0.1:7777/api/proxies/nas
```

`GET /api/proxies` returns the visitor token **in plain text** in
`auth_header` (`null` when authentication is off). The management responses are
sent with `Cache-Control: no-store`, but the whole management plane must still
be reached over HTTPS/TLS: anyone who can read the API response or the management
token can use the tunnel. A front proxy must not log response bodies.

`PUT` replaces the whole entry. Omitting `auth_header`, sending `null`, or
sending an empty string disables visitor authentication; a non-empty value sets
or rotates it. The value is trimmed and may not contain line breaks or exceed
8192 bytes. The web page pre-fills the current value, so editing a domain or
address from the page keeps the token; scripts that change only
`domain`/`local_addr` must include the current `auth_header` to keep it.

A change is validated locally, written atomically to `ht.toml`, applied to the
running server's proxy list, and published on the route watch. An upstream
session therefore mirrors or withdraws the route immediately, without a restart.

## The upstream modification switch

`PUT /api/settings/allow-upstream-modify` changes this process's own switch. It
does not accept `node` or `session`:

```sh
curl -X PUT -H "Host: edge.lan" -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"allowed":true}' \
  http://127.0.0.1:7777/api/settings/allow-upstream-modify
```

The response is `{"allow_upstream_modify":true}`. The value is written to
`ht.toml` and published to the running upstream session, which reconnects at once
without the retry backoff so the next hop receives the new capability bits in a
fresh `Hello`. A `PUT` is rejected with `409` when:

- there is no `[upstream]` (a standalone ingress has nothing to authorize);
- `HT_ALLOW_UPSTREAM_MODIFY` owns the value, so a file write would be
  immediately overridden;
- `HT_PROXIES` owns the local services and already keeps them read-only.

`GET /api/status` reports:

```json
{"allow_upstream_modify":false,"allow_upstream_modify_editable":true}
```

`allow_upstream_modify_editable` is the page's hint: it is `true` only when an
upstream exists, the environment does not own the value, and `HT_PROXIES` is not
managing the local services. The page shows the current value, disables the
control with a reason when it is not editable, and shows "无上游" on a standalone
ingress.

## Downstream node proxies

The same proxy endpoints operate on a connected downstream node when a `node`
or `session` query parameter is present:

```text
GET    /api/proxies?node=home
GET    /api/proxies?session=3
PUT    /api/proxies/nas?node=home
DELETE /api/proxies/nas?session=3
```

Without either parameter the request manages the local process, exactly as
before. Both parameters may be given; they must identify the same session,
which also protects against a reconnect overlap where the old and the new
session use the same node name. A node name is not a unique key: zero matches
return `404`, several matches return `409` with the candidate session ids in
the error (use `session` to disambiguate).

The response is the same array as the local endpoint. Each session listed by
`GET /api/sessions` carries:

- `manageable`: the node understands proxy management requests. It may still
  list its services when `writable` is `false`.
- `writable`: its local services may be changed. It is `false` when the node
  turns `allow_upstream_modify` off (the default) or runs with `HT_PROXIES`.
- `proxy_source`: `file` when the node reads `[[proxies]]` from its `ht.toml`,
  `env` when `HT_PROXIES` owns them. It does not change when only the
  `allow_upstream_modify` switch is off.

`allow_upstream_modify` is `false` by default. The entry may list a downstream
node's services (including `auth_header`) but a `PUT` or `DELETE` returns `409`.
The node can turn the switch on or off from its own management page, which writes
`allow_upstream_modify` to its `ht.toml` and reconnects its upstream session at
once so the new capability is announced. The routes of that node are withdrawn
and advertised again during the reconnect. When `HT_PROXIES` owns the local
services the switch is disabled, because writes are already impossible.

A downstream operation is executed by the node itself with the same code as its
own management page: the change is validated, written atomically to that node's
`ht.toml`, applied to its running proxy list, and published on its route watch.
The entry node therefore sees the added or withdrawn route without a restart.

The proxy management frames carry `auth_header` now, which is a wire-format
change inside v4. All nodes on one control plane must run the same release. A
new entry node refuses management of a downstream that does not advertise
`CAP_PROXY_AUTH` and answers `409` with an upgrade hint, instead of letting the
session fail to decode the frame.

Remote management is off by default when both processes are built with the
`management` feature. A node opts in with `allow_upstream_modify = true` or with
its own management page; only then may the previous hop edit its `ht.toml`.
Anyone who can reach the entry `control_host` and knows its token can still list
the connected downstream nodes' services, including `auth_header`; run only
trusted nodes on that control plane, or build without `management` to remove the
feature.

Failures:

| Status | Meaning |
| --- | --- |
| `400` | The session id is not a number, `node` and `session` disagree, or the input is invalid |
| `404` | No session matches the selector, or the proxy does not exist on the node |
| `409` | The node is not manageable, does not allow upstream writes (`allow_upstream_modify = false`), is `HT_PROXIES` read-only, predates the `auth_header` wire shape, or several sessions share the name |
| `502` | The node rejected the change (for example a domain conflict or an unwritable file) |
| `504` | The node did not answer within 10 seconds; the change may still have been applied |

## Disconnecting a downstream node

```sh
curl -X DELETE -H "Host: edge.lan" -H "Authorization: Bearer $TOKEN" \
  http://127.0.0.1:7777/api/sessions/3
```

A successful `DELETE /api/sessions/{id}` withdraws the session's routes, closes
the connection and returns `204`. This is a temporary disconnect: the node
reconnects according to its own `retry_interval`. Unknown or already closed
sessions return `404`.

## Errors

API failures are JSON: `{"error":"...","request_id":"..."}`. A path the router
does not know is a `404` that names the method and path, and carries the same
request id in the body and in the `x-http-tunnel-request-id` header:

```json
{"error":"No such management API endpoint: GET /api/nope","request_id":"0123456789abcdef"}
```

An unknown path outside `/api` is answered with an HTML page that names the path
and the request id. Both are logged at `warn` with the same id, so a typo, a
stale bookmark, or a request that never reached `ht` can be told apart by the
presence of `x-http-tunnel-request-id`.

## Environment-managed proxies

If `HT_PROXIES` is set, the local services come from the environment and are
read-only at runtime:

- `/api/status` reports `"proxies_managed": true` and lists `HT_PROXIES` among
  the set variables; the page hides the add, edit and delete actions.
- `PUT` and `DELETE /api/proxies/*` return `409 Conflict`.
- The `allow_upstream_modify` control is disabled, because the services are
  already read-only.
- Nothing is written to `ht.toml`.

Scalar environment overrides (`HT_TOKEN`, `HT_UPSTREAM_URL`, ...) also win at runtime
but are never persisted by an API change: the file baseline is saved, and the
environment is reapplied to the effective configuration.
