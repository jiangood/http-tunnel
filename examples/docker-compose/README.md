# Docker Compose Examples

This directory lists Docker Compose files that run `http-tunnel` in a container, one directory per role:

- [`server/`](./server) runs the server. Put your `server.toml` next to its `docker-compose.yml` and start it with
  `docker compose up -d`.
- [`client/`](./client) runs the client on the host behind the NAT.

The published image (`ghcr.io/jiangood/http-tunnel`) is distroless: it only contains the binary and has no shell. It
runs as root, so it can write the mounted directory without any `chown` on the host.

The server holds the whole configuration of a deployment, so it reads `server.toml` from its working directory
(`/app` in the image). The directory is mounted read-write because the server generates a minimal `server.toml` if
it's missing, and the [administration API](../../README.md#administration-api) rewrites it in place. If the folder is
read-only, pass an explicit path to a writable location with `command: server /path/to/server.toml`.

A client is configured by the server, so it only takes the address of the server, its name and its token, which are
passed through the `HTTP_TUNNEL_REMOTE`, `HTTP_TUNNEL_NAME` and `HTTP_TUNNEL_TOKEN` environment variables.

## Server

Copy `server/docker-compose.yml` and your `server.toml` into the same directory, adjust the published ports to the
config you use, and run:

```bash
cd server
docker compose up -d
```

The three published ports map the `server_port` (clients), the `http_port` (visitors) and the `api_port`
(administration API). The API port is published on every interface of the host and can be dropped if the API is not
enabled. Set `api_port = 2335` in `server.toml` so the container accepts the forwarded connections,
and restrict the access with a reverse proxy, a firewall, or a private network.

A minimal `server.toml` is provided in [`../minimal/server.toml`](../minimal/server.toml).

## Client

Edit `client/docker-compose.yml` and set `HTTP_TUNNEL_REMOTE` to the address of your server and `HTTP_TUNNEL_NAME` to
the name of the client as configured in `server.toml`. The token is read from `HTTP_TUNNEL_TOKEN`; put all three in a
`.env` file next to the docker-compose file rather than hardcoding them:

```dotenv
# .env
HTTP_TUNNEL_REMOTE=myserver.com:2333
HTTP_TUNNEL_NAME=home_nas
HTTP_TUNNEL_TOKEN=use_a_secret_that_only_you_know
```

Then start it on the host behind the NAT:

```bash
cd client
docker compose up -d
```

The default bridge network is enough: the client only dials the server. A tunnel that reaches a service on the host
uses `host.docker.internal` as its `local_addr` in `server.toml` (on Linux, add
`extra_hosts: ["host.docker.internal:host-gateway"]` to the service), or the name of another container on the same
network. The client's [administration API](../../README.md#client-administration-api) listens on `8610`, which the
example publishes; drop the mapping if you don't need it, or change it with `HTTP_TUNNEL_API_PORT`.

