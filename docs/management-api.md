# Web management and administration API

The universal `ht` process exposes a small web page and a REST API on the
same port. It manages the local `[[proxies]]` of the running process:

- A standalone ingress serves them directly.
- A leaf or relay also mirrors them to its `[upstream]`.

The management interface is enabled by the reserved `[server].domain` and shares
the main `[server].port`; it has no listener or port of its own. A process without
`[server]`, or with a server but no `domain`, has no management page.

```toml
[server]
port = 7777
bind = "127.0.0.1"
domain = "edge.lan"
```

Open `http://<server.domain>:<server.port>/`. Requests whose `Host` is not
`[server].domain` keep following the normal proxy/v4 routing, so a business
domain never reaches the management router.

The page itself is protected by the browser-native HTTP Basic login: username
`ht`, password = the listener `token`. It is the only Basic-authenticated
resource. It is a single HTML document with an inline stylesheet and script and
no external dependency; it renders nothing on the server and reads all of its
data from `/api` with the Bearer token it is handed after login. One node
card lists the local process together with every connected downstream v4
session. Every row offers "管理服务", which opens a dialog that lists, adds,
edits and deletes that node's own `[[proxies]]`; downstream rows also offer
"断开", which closes that one session. A local proxy can also be probed with
"测试", which dials its `local_addr` from the node that owns it. The route and
node tables have client-side filters, and the status card shows active/total
visitors and the last reload result. The page also edits every setting of the
local node (except `[[proxies]]`, which has its own dialog), followed by the API
list and a reference table of the `ht.toml` keys and their defaults. Saving
preserves the existing file's comments and layout, and "导出配置" downloads the
file for backup.

## API

Every API call uses a Bearer token, which is also the listener `token`:

| Method | Path | Description |
| --- | --- | --- |
| GET | `/api/status` | Name, transport, upstream, connectivity, proxy count, session count, active/total visitors, reload status, `allow_upstream_modify` and whether it is editable, version, config path, uptime |
| GET | `/api/settings` | Every setting of the local node, including tokens; `[[proxies]]` is excluded |
| GET | `/api/settings/export` | Download the on-disk `ht.toml` as `text/toml` with a `Content-Disposition` attachment; comments and tokens are included |
| GET | `/api/proxies` | List local `[[proxies]]` as `name`/`domain`/`local_addr`/`auth_header`/`source`; with `?node=` or `?session=`, list the proxies of that downstream node |
| GET | `/api/routes` | The effective routing table, including v4 owners |
| GET | `/api/sessions` | The connected downstream v4 sessions: session id, node, version, peer address, forwarded client address, role, uptime, owned domains, `manageable`, `writable` and `proxy_source` |
| PUT | `/api/settings` | Replace every setting of the local node and write it to `ht.toml`; `?dry_run=true` only checks, `?restart=true` rebuilds the runtime when needed |
| POST | `/api/restart` | Rebuild the listener and upstream session from the `ht.toml` on disk |
| POST | `/api/proxies/{name}/probe` | Dial the saved `local_addr` of a local proxy with a 3 s timeout; returns `ok`, `latency_ms` and an optional `error` |
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

## Node settings

`GET /api/settings` returns every setting of the local node. Unlike the proxy
endpoint it includes the listener token and the upstream token in plain text;
the page is only served after Basic authentication with that token, and every
management response is `no-store`.

```json
{
  "name":"home",
  "server":{
    "port":7777,
    "bind":"127.0.0.1",
    "domain":"127.0.0.1",
    "token":"secret",
    "keepalive_interval":15,
    "max_pending_handshakes":256,
    "max_sessions":256
  },
  "upstream":{
    "url":"wss://control.example.com:443",
    "token":"upstream-secret",
    "allow_modify":false,
    "retry_interval":1,
    "keepalive_interval":15
  }
}
```

`server` is `null` for a pure client. `PUT /api/settings` replaces the whole
object above. `[[proxies]]` is not part of it because each entry has its own
endpoints. The body is validated before anything is written; a missing or invalid
field returns `400`. `server.bind` must be an IPv4/IPv6 literal; use `127.0.0.1`
behind a local reverse proxy and `0.0.0.0` to listen on every interface. A change
that only touches `upstream.allow_modify` is applied to the running session at
once. Any other change is saved and picked up by the file watcher, which rebuilds
the listener and/or upstream session through the normal hot-reload path; existing
visitors and downstream sessions may disconnect briefly. A same-port `bind`
change stops the old listener, binds the new address, and restores the previous
listener if the new address is unusable.

The page does not save a restart-requiring change immediately. It first calls:

```sh
curl -X PUT -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"name":"home","server":{"port":7777,"bind":"127.0.0.1","domain":"home.lan"},...}' \
  'http://127.0.0.1:7777/api/settings?dry_run=true'
```

A dry run validates and compares without writing. It returns
`"saved":false` and `"restart_required":true|false`. After the operator confirms,
the page saves with `?restart=true`, which schedules the rebuild immediately
instead of waiting for the file watcher:

```json
{"restart_required":true,"restart_triggered":true,"saved":true,"settings":{ "...": "the saved values" }}
```

Without `?restart=true`, a restart-requiring change is still picked up by the
file watcher within about a second.

Saving through the API preserves the comments and layout of the existing
`ht.toml`. Values are merged in place; `[[proxies]]` entries are matched by
`name`, so editing one entry keeps the comments around it, adding an entry
appends a new table, and deleting one removes it. If the existing file cannot be
parsed (for example while an operator is halfway through an edit), the API
writes the normalized TOML instead so the node remains recoverable.

`POST /api/restart` rebuilds the listener and upstream session from the
configuration on disk, regardless of whether it changed. It is the button behind
"重启运行时" and returns `{"restarting":true}` before the runtime is stopped, so
the HTTP response can be flushed first. The `ht` process itself never exits.

Changing `server.token` changes the password of the management page; changing
`server.port` or `server.domain` moves the page. They take effect after the
rebuild, so reload the page (and log in with the new token) after the response.
Removing `server` or its `domain` also removes the management page.

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
`[upstream].allow_modify` in `ht.toml` and published to the running upstream
session, which reconnects at once without the retry backoff so the next hop
receives the new capability bits in a fresh `Hello`. A `PUT` is rejected with
`409` when there is no `[upstream]` (a standalone ingress has nothing to
authorize).

`GET /api/status` reports the current switch and, when applicable, the reload
diagnostics:

```json
{
  "allow_upstream_modify":false,
  "allow_upstream_modify_editable":true,
  "active_visitors":2,
  "total_visitors":148,
  "last_reload_error":"The configuration file is invalid: ...",
  "last_reload_error_seconds_ago":12,
  "last_reload_success_seconds_ago":3
}
```

`allow_upstream_modify_editable` is the page's hint: it is `true` only when an
upstream exists. The page shows the current value, disables the control when it
is not editable, and shows "无上游" on a standalone ingress.

`active_visitors` counts visitor connections currently accepted; `total_visitors`
counts visitor connections routed since the current runtime started (a hot
reload or manual restart resets it). Management and v4 control connections are
excluded. The three `last_reload_*` fields are omitted
when there is nothing to report: `last_reload_error` is set after a file read,
parse, bind or apply failure, and clears on the next successful reload.
`last_reload_error_seconds_ago` and `last_reload_success_seconds_ago` make it
easy to render a relative time.

## Probing a local service

`POST /api/proxies/{name}/probe` dials the saved `local_addr` from this node's
perspective. It is read-only and local-only; it does not accept `node` or
`session` because a downstream probe needs a new v4 frame.

```json
{"name":"nas","local_addr":"127.0.0.1:5000","ok":true,"latency_ms":1}
```

A connection failure is still `200`:

```json
{"name":"nas","local_addr":"127.0.0.1:5000","ok":false,"latency_ms":0,"error":"Failed to connect to local TCP service 127.0.0.1:5000: Connection refused"}
```

An unknown service name is `404`. The timeout is 3 seconds. The probe connects
and immediately closes the connection; it does not send HTTP.

## Exporting the configuration

`GET /api/settings/export` returns the current `ht.toml` bytes with
`Content-Type: text/toml; charset=utf-8` and
`Content-Disposition: attachment; filename="ht.toml"`. Unlike `GET
/api/settings`, it preserves comments and formatting because it reads the file
directly. It contains tokens in plain text, so the same HTTPS/no-logging rules
as the rest of the management API apply.

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
  leaves `[upstream].allow_modify` off (the default).
- `proxy_source`: `file` for a current node. An older downstream peer that still
  announces the removed `HT_PROXIES` capability is reported as `env` and is
  read-only. It does not change when only the `[upstream].allow_modify` switch
  is off.

`[upstream].allow_modify` is `false` by default. The entry may list a downstream
node's services (including `auth_header`) but a `PUT` or `DELETE` returns `409`.
The node can turn the switch on or off from its own management page, which writes
`[upstream].allow_modify` to its `ht.toml` and reconnects its upstream session at
once so the new capability is announced. The routes of that node are withdrawn
and advertised again during the reconnect.

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
`management` feature. A node opts in with `[upstream].allow_modify = true` or
with its own management page; only then may the previous hop edit its `ht.toml`.
Anyone who can reach the entry `[server].domain` and knows its token can still
list the connected downstream nodes' services, including `auth_header`; run only
trusted nodes on that control plane, or build without `management` to remove the
feature.

Failures:

| Status | Meaning |
| --- | --- |
| `400` | The session id is not a number, `node` and `session` disagree, or the input is invalid |
| `404` | No session matches the selector, or the proxy does not exist on the node |
| `409` | The node is not manageable, does not allow upstream writes (`[upstream].allow_modify = false`), is a legacy read-only peer, predates the `auth_header` wire shape, or several sessions share the name |
| `502` | The node rejected the change (for example a domain conflict or an unwritable file) |
| `504` | The node did not answer within 10 seconds; the change may still have been applied |

## Disconnecting a downstream node

```sh
curl -X DELETE -H "Host: edge.lan" -H "Authorization: Bearer $TOKEN" \
  http://127.0.0.1:7777/api/sessions/3
```

A successful `DELETE /api/sessions/{id}` withdraws the session's routes, closes
the connection and returns `204`. This is a temporary disconnect: the node
reconnects according to its own `[upstream].retry_interval`. Unknown or already closed
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

## Configuration source

`ht.toml` is the only configuration source. The management page and
`PUT /api/settings` write that same file. On first start a missing file is
replaced with a commented example that runs as a server (`[server]` enabled on
`0.0.0.0:7777` with a random `token`, `[upstream]` commented out); the process
keeps starting with it and hot-reloads later edits. There is no environment-only
mode.
