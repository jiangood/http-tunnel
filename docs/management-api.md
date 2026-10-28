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

The browser-native HTTP Basic login uses username `ht` and the listener `token`
as the password. The page is plain HTML with no CSS or JavaScript.

## API

The API uses a Bearer token, which is also the listener `token`:

| Method | Path | Description |
| --- | --- | --- |
| GET | `/api/status` | Name, transport, upstream, connectivity, proxy count |
| GET | `/api/proxies` | List local `[[proxies]]` as `name`/`domain`/`local_addr` |
| PUT | `/api/proxies/{name}` | Create or replace a local proxy |
| DELETE | `/api/proxies/{name}` | Delete a local proxy |

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

## Environment-managed proxies

If `HT_PROXIES` is set, the local services come from the environment and are
read-only at runtime:

- The web form is hidden.
- `PUT` and `DELETE /api/proxies/*` return `409 Conflict`.
- Nothing is written to `ht.toml`.

Scalar environment overrides (`HT_TOKEN`, `HT_UPSTREAM_URL`, ...) also win at runtime
but are never persisted by an API change: the file baseline is saved, and the
environment is reapplied to the effective configuration.
