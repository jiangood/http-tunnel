# Client Administration API

The client exposes a small REST API to maintain its own tunnels at runtime. It listens on `8610` by default; change the
port with `--api-port` or the `HTTP_TUNNEL_API_PORT` environment variable, or set it to `0` to disable the API:

```bash
./http-tunnel client --remote myserver.com:2333 --name home_nas \
  --token use_a_secret_that_only_you_know --api-port 2336
```

Every route requires `Authorization: Bearer <token>`, reusing the token that the client already authenticates with, so
there is no second secret to manage. The API binds to `0.0.0.0`, which makes it reachable from the network, so protect
it the same way as the [server API](./admin-api.md): a reverse proxy that terminates TLS, a firewall rule, or a private
network such as WireGuard. A binary built without the `client-api` feature (on by default) has no API.

## Resources

A tunnel belongs to a client: it maps a single `Host` domain to a `local_addr` on the client side. The server remains
the source of truth. The client API does not keep a configuration of its own: a change is forwarded to the server over
the config channel, which validates it against the whole configuration, persists it, and pushes the resulting config
back. The client API answers only once the server has given its verdict.

## Endpoints

| Method | Path | Body | Description |
| --- | --- | --- | --- |
| `GET` | `/api/status` | | The name and address of the client, whether it is connected, and its tunnel count |
| `GET` | `/api/tunnels` | | List the tunnels the client is serving |
| `PUT` | `/api/tunnels/{domain}` | `{local_addr}` | Create or replace a tunnel |
| `DELETE` | `/api/tunnels/{domain}` | | Delete a tunnel |

`{domain}` is lowercased before it is sent to the server. A rejected change is reported with the status that matches
the reason: `404` when the tunnel does not exist, `409` when the domain is already used, `400` for a malformed request,
`503` when the client is not connected to the server, and `504` when the server does not answer in time.

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

1. The client forwards the change to the server over its config channel.
2. The server validates it against the whole configuration, so the global rules (unique domains and unique tokens)
   always hold, and writes the configuration back to its `server.toml` atomically.
3. The server pushes the new config to the client.
4. The client starts the new tunnels, stops the removed ones, and restarts the ones whose `local_addr` changed,
   without a restart of either side. The answer to the API request carries the outcome.

## Caveats

- The server does not restrict *which* free domain a client may claim: any holder of the client token can add a tunnel
  for any domain that no other client owns, and change or remove the client's own tunnels. The token is already the
  client's credential, so treat it as a secret.
- Changing a `local_addr` restarts the corresponding tunnel, so in-flight visitors on it are cut off.
- The client must be connected to the server for a change to be accepted; there is no offline queue.
