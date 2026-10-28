# The release binary is a static musl build, so the image needs no base OS.
# Using `scratch` also avoids pulling a distro image only to copy one file.
FROM scratch AS ht
ARG TARGETARCH=amd64
WORKDIR /app
COPY --chmod=0755 build-out/${TARGETARCH}/ht /usr/local/bin/ht
# Runs as root so a first run can generate /app/ht.toml and the web API can
# persist it. For a non-root deployment, make the mounted config writable and
# pass `--user 65532:65532` (or the equivalent in compose/Kubernetes).
# The listener is configured by `server_port`; the web API is disabled unless
# `web_port` is set. These are documentation-only defaults.
EXPOSE 9000 8610
# Docker stops a container with SIGTERM. `ht` handles it like SIGINT and drains
# the in-flight visitors; allow at least the 30 s drain in `docker stop`.
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/ht"]
