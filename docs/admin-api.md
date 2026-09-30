# Administration API

The server can expose a REST API and a minimal web UI to manage the clients and their services at runtime. The API
is disabled by default and is enabled by setting both `api_bind_addr` and `api_token` in `server.toml`:

```toml
api_bind_addr = "127.0.0.1:2335"
api_token = "a_secret_for_the_admin_api"
```

If `api_bind_addr` is set without `api_token`, the server refuses to start. Every request to `/api/*` must carry the
header `Authorization: Bearer <api_token>`; otherwise it is answered with `401 Unauthorized`. `GET /` serves a
minimal web UI which asks for the token and keeps it in the browser's `localStorage`.

## Resources

A client is identified by its name and holds a token. A service belongs to a client: it is a tunnel from a set of
`Host` values to a `local_addr` on the client side. The client names and the service names are global, and a `Host`
can only be claimed by one service.

## Endpoints

| Method | Path | Body | Description |
| --- | --- | --- | --- |
| `GET` | `/api/status` | | Client and service counts, and the listening addresses |
| `GET` | `/api/clients` | | List the clients and their services |
| `POST` | `/api/clients` | `{name, token, heartbeat_timeout?, retry_interval?, nodelay?}` | Create a client. It may have no service yet |
| `GET` | `/api/clients/{client}` | | Get a client |
| `PATCH` | `/api/clients/{client}` | `{token?, heartbeat_timeout?, retry_interval?, nodelay?}` | Update a client |
| `DELETE` | `/api/clients/{client}` | | Delete a client and all its services |
| `GET` | `/api/clients/{client}/services` | | List the services of a client |
| `PUT` | `/api/clients/{client}/services/{service}` | `{hosts, local_addr, nodelay?, retry_interval?}` | Create or replace a service |
| `DELETE` | `/api/clients/{client}/services/{service}` | | Delete a service |

The tokens are always masked as `****` in the responses.

## Examples

```bash
TOKEN=a_secret_for_the_admin_api
BASE=http://127.0.0.1:2335

# Create a client
curl -X POST "$BASE/api/clients" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"name":"home","token":"a_secret_token"}'

# Add a service to it
curl -X PUT "$BASE/api/clients/home/services/nas" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"hosts":["nas.example.com"],"local_addr":"127.0.0.1:80"}'

# List the clients
curl "$BASE/api/clients" -H "Authorization: Bearer $TOKEN"

# Delete the service, then the client
curl -X DELETE "$BASE/api/clients/home/services/nas" -H "Authorization: Bearer $TOKEN"
curl -X DELETE "$BASE/api/clients/home" -H "Authorization: Bearer $TOKEN"
```

## How a change takes effect

1. The change is validated against the whole configuration, so the global rules (unique service names, unique hosts,
   unique tokens) always hold.
2. The configuration is written back to the `server.toml` atomically. If the write fails, the runtime is left
   untouched.
3. The routing table, the service map and the client map are rebuilt.
4. The tunnels of the removed or changed services are dropped.
5. The new client config is pushed to the connected clients over their config channel. Each client starts the new
   services, stops the removed ones, and restarts the ones whose `local_addr`, `nodelay`, `retry_interval` or
   heartbeat timeout changed, without a restart.

## Caveats

- The `server.toml` is rewritten as a whole, so its comments are not preserved once it's managed through the API.
- Changing the `token` of a client only affects the server. The connected client keeps using the token it was started
  with, so a client must be restarted with the new `--token`. The config channel is authenticated by the token, so a
  client that reconnects with an old token is rejected.
- A client with no service is valid and simply has nothing to forward.
