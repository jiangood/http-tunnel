# Static musl binaries are assembled into small, role-specific images.
FROM gcr.io/distroless/static-debian12 AS base
ARG TARGETARCH
WORKDIR /app

FROM base AS server
COPY --chmod=0755 build-out/${TARGETARCH}/hts /usr/local/bin/hts
ENTRYPOINT ["/usr/local/bin/hts"]

FROM base AS client
COPY --chmod=0755 build-out/${TARGETARCH}/htc /usr/local/bin/htc
ENTRYPOINT ["/usr/local/bin/htc"]

# A node is a server and a client in one process: one image for one relay hop.
FROM base AS node
COPY --chmod=0755 build-out/${TARGETARCH}/htn /usr/local/bin/htn
ENTRYPOINT ["/usr/local/bin/htn"]
