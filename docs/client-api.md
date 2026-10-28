# Web management and administration API

The universal `ht` process can expose a small web page and a REST API on the
same port. It manages the local `[[proxies]]` of the running process:

- A standalone ingress serves them directly.
- A leaf or relay also advertises them to its `[upstream]` according to
  `mirror`/`claim`.

The management interface is disabled by default. Set `web_port` in `ht.toml` to
enable it; `web_host` selects the bind address and defaults to `0.0.0.0`.

```toml
web_port = 8610
web_host = "127.0.0.1"
```

`HT_WEB_PORT` and `HT_WEB_HOST` override these values. `web_port = 0` disables
the interface.

Open `http://<process address>:8610/`. The browser-native HTTP Basic login uses
username `ht` and the listener `token` as the password. The page is plain HTML
with no CSS or JavaScript.

## API

The API uses a Bearer token, which is also the listener `token`:

| Method | Path | Description |
| --- | --- | --- |
| GET | `/api/status` | Name, transport, upstream, connectivity, proxy count |
| GET | `/api/tunnels` | List local `[[proxies]]` as `name`/`domain`/`local_addr` |
| PUT | `/api/tunnels/{name}` | Create or replace a local proxy |
| DELETE | `/api/tunnels/{name}` | Delete a local proxy |

The paths keep the historical name `tunnels`; the configuration and the storage
section are `[[proxies]]`.

```sh
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8610/api/tunnels
curl -X PUT -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"domain":"nas.example.com","local_addr":"127.0.0.1:80"}' \
  http://127.0.0.1:8610/api/tunnels/nas
```

A change is validated locally, written atomically to `ht.toml`, applied to the
running server's proxy list, and published on the route watch. An upstream
session therefore mirrors or withdraws the route immediately, without a restart.

## Environment-managed proxies

If `HT_PROXIES` is set, the local services come from the environment and are
read-only at runtime:

- The web form is hidden.
- `PUT` and `DELETE /api/tunnels/*` return `409 Conflict`.
- Nothing is written to `ht.toml`.

Scalar environment overrides (`HT_TOKEN`, `HT_SERVER`, ...) also win at runtime
but are never persisted by an API change: the file baseline is saved, and the
environment is reapplied to the effective configuration.
