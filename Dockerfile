# The musl binaries are built by the release workflow and passed in as build args.
# `TARGETARCH` is provided by BuildKit (amd64 or arm64), so the Dockerfile only
# assembles the image instead of cross-compiling Rust inside it.
FROM gcr.io/distroless/static-debian12

ARG TARGETARCH
# The binary is installed outside the working directory: the server defaults to
# `server.toml` in the working directory, so a directory is mounted over `/app`,
# and keeping the binary elsewhere stops that mount from hiding it:
#   docker run -v "$PWD:/app" ... http-tunnel server
# The mode is set explicitly: the release workflow gets the binaries through
# upload-artifact/download-artifact, which do not preserve the executable bit.
# The distroless stage has no shell, so it cannot chmod after the copy.
COPY --chmod=0755 build-out/${TARGETARCH}/http-tunnel /usr/local/bin/http-tunnel
WORKDIR /app
USER 1000:1000
ENTRYPOINT ["/usr/local/bin/http-tunnel"]
