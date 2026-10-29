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
data from `/api` with the Bearer token it is handed after login.

## API

Every API call uses a Bearer token, which is also the listener `token`:

| Method | Path | Description |
| --- | --- | --- |
| GET | `/api/status` | Name, transport, upstream, connectivity, proxy count, version, config path, uptime |
| GET | `/api/proxies` | List local `[[proxies]]` as `name`/`domain`/`local_addr` |
| GET | `/api/routes` | The effective routing table, including v4 owners |
| PUT | `/api/proxies/{name}` | Create or replace a local proxy |
| DELETE | `/api/proxies/{name}` | Delete a local proxy |

`GET /api/routes` shows the table the visitor path actually uses. A local
service and a downstream session can claim the same domain; the local service
wins, but both entries are listed:

```json
[
  {"domain":"app.example.com","owner":"local","name":"app","target":"127.0.0.1:3000"},
  {"domain":"svc.example.com","owner":"v4","session_id":3,"node":"home"}
]
```

The same table is logged at `info` whenever it changes:

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
  -d '{"domain":"nas.example.com","local_addr":"127.0.0.1:80"}' \
  http://127.0.0.1:7777/api/proxies/nas
```

A change is validated locally, written atomically to `ht.toml`, applied to the
running server's proxy list, and published on the route watch. An upstream
session therefore mirrors or withdraws the route immediately, without a restart.

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

- `/api/status` reports `"proxies_managed": true`, and the page hides the form
  and the delete buttons.
- `PUT` and `DELETE /api/proxies/*` return `409 Conflict`.
- Nothing is written to `ht.toml`.

Scalar environment overrides (`HT_TOKEN`, `HT_UPSTREAM_URL`, ...) also win at runtime
but are never persisted by an API change: the file baseline is saved, and the
environment is reapplied to the effective configuration.
