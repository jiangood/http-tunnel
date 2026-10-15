# Client Administration API

The client exposes a small REST API to maintain its own tunnels at runtime. It listens on `8610` by default; change the
port with `--api-port` or the `HTTP_TUNNEL_API_PORT` environment variable, or set it to `0` to disable the API:

```bash
./http-tunnel --config client.toml --api-port 2336
```

Every route requires `Authorization: Bearer <token>`, reusing the shared token that the client authenticates with, so
there is no second secret to manage. The API binds to `0.0.0.0`, which makes it reachable from the network, so protect
it with a reverse proxy that terminates TLS, a firewall rule, or a private network such as WireGuard. A binary built
without the `client-api` feature (on by default) has no API.

## Resources

A tunnel maps a single `Host` domain to a `local_addr` on the client side. The client is the source of truth: a change
is validated locally, written back to the client configuration file, and applied to the running control channels
without a restart of either side. The handlers answer once the change is persisted.

## Endpoints

| Method | Path | Body | Description |
| --- | --- | --- | --- |
| `GET` | `/api/status` | | The name and address of the client, whether it is connected, and its tunnel count |
| `GET` | `/api/tunnels` | | List the tunnels the client is serving |
| `PUT` | `/api/tunnels/{domain}` | `{local_addr}` | Create or replace a tunnel |
| `DELETE` | `/api/tunnels/{domain}` | | Delete a tunnel |

`{domain}` is lowercased before it is applied. A rejected change is reported with the status that matches the reason:
`404` when the tunnel does not exist, `400` for a malformed domain or an empty `local_addr`, and `500` when the
configuration file could not be written.

## Examples

```bash
TOKEN=use_a_secret_that_only_you_know
BASE=http://127.0.0.1:2336

# Add a tunnel
curl -X PUT "$BASE/api/tunnels/nas.example.com" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"local_addr":"127.0.0.1:80"}'

# List the tunnels
curl "$BASE/api/tunnels" -H "Authorization: Bearer $TOKEN"

# Delete the tunnel
curl -X DELETE "$BASE/api/tunnels/nas.example.com" -H "Authorization: Bearer $TOKEN"
```

## How a change takes effect

1. The client validates the change (a lowercased, well-formed domain and a non-empty `local_addr`).
2. It writes the new configuration back to its `client.toml` atomically.
3. It starts the new tunnels, stops the removed ones, and restarts the ones whose `local_addr` changed, without a
   restart of either side. The answer to the API request carries the outcome.
4. A newly started tunnel opens a control channel, which registers its domain on the server.

## Caveats

- The configuration file must be writable. The API writes back to the path the client was started with (default
  `client.toml`), so a container has to mount it read-write.
- Only the tunnels are written back. If the identity (`remote`, `name`, `token`) was given on the command line rather
  than in the file, it is not persisted: keep passing it on the command line, or put it in the file.
- The shared token is the client's credential and, since it is shared, the server does not restrict *which* free
  domain a client may claim. Any holder of the token can add a tunnel for any domain that no other client serves, and
  change or remove the client's own tunnels. Treat the token as a secret.
- Changing a `local_addr` restarts the corresponding tunnel, so in-flight visitors on it are cut off.
- A change made through the API only takes effect on the local client. A hand edit of `client.toml` still needs a
  restart of the client.
