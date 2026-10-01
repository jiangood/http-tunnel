# The musl binaries are built by the release workflow and passed in as build args.
# `TARGETARCH` is provided by BuildKit (amd64 or arm64), so the Dockerfile only
# assembles the image instead of cross-compiling Rust inside it.
FROM gcr.io/distroless/static-debian12

ARG TARGETARCH
# The server defaults to `server.toml` in the working directory, so running
# `server` only needs a bind mount of the directory that holds it:
#   docker run -v "$PWD:/app" ... http-tunnel server
WORKDIR /app
# The mode is set explicitly: the release workflow gets the binaries through
# upload-artifact/download-artifact, which do not preserve the executable bit.
# The distroless stage has no shell, so it cannot chmod after the copy.
COPY --chmod=0755 build-out/${TARGETARCH}/http-tunnel .
USER 1000:1000
ENTRYPOINT ["./http-tunnel"]
