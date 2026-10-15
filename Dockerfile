# The musl binaries are built by the release workflow and passed in as build args.
# `TARGETARCH` is provided by BuildKit (amd64 or arm64), so the Dockerfile only
# assembles the image instead of cross-compiling Rust inside it.
FROM gcr.io/distroless/static-debian12

ARG TARGETARCH
# The binary is installed outside the working directory, so that a client that
# mounts a directory over `/app` (to hold its `client.toml`) doesn't hide it. The
# server takes no configuration file and needs no mount, only its ports and token:
#   docker run -e HTTP_TUNNEL_TOKEN=... -p 2333:2333 -p 80:80 ... server
# Server ports default to 2333 (client connections) and 80 (HTTP entrypoint).
# The mode is set explicitly: the release workflow gets the binaries through
# upload-artifact/download-artifact, which do not preserve the executable bit.
# The distroless stage has no shell, so it cannot chmod after the copy.
COPY --chmod=0755 build-out/${TARGETARCH}/http-tunnel /usr/local/bin/http-tunnel
WORKDIR /app
# Runs as root so that a client can create and rewrite `client.toml` in a bind
# mount whose owner is the host user (usually root) when its administration API
# is used. Override with `--user` when the mounted directory is already writable
# by the intended user.
ENTRYPOINT ["/usr/local/bin/http-tunnel"]
