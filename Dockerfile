# The musl binaries are built by the release workflow and passed in as build args.
# `TARGETARCH` is provided by BuildKit (amd64 or arm64), so the Dockerfile only
# assembles the image instead of cross-compiling Rust inside it.
FROM gcr.io/distroless/static-debian12

ARG TARGETARCH
WORKDIR /app
COPY build-out/${TARGETARCH}/http-tunnel .
USER 1000:1000
ENTRYPOINT ["./http-tunnel"]
