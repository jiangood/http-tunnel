# The musl binaries are built by the release workflow and passed in as build args.
# TARGETARCH is provided by BuildKit (amd64 or arm64).
FROM gcr.io/distroless/static-debian12 AS base

ARG TARGETARCH
COPY --chmod=0755 build-out/${TARGETARCH}/http-tunnel /usr/local/bin/http-tunnel
WORKDIR /app

# Dedicated server image: starts the server without a command.
FROM base AS server
ENTRYPOINT ["/usr/local/bin/http-tunnel", "server"]

# Dedicated client image: starts the client and reads /app/client.toml by default.
FROM base AS client
ENTRYPOINT ["/usr/local/bin/http-tunnel"]
