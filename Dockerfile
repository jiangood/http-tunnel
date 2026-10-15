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
