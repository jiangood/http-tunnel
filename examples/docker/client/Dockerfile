# Optional: bake the config into an image instead of mounting ./data.
# The compose.yaml in this directory uses the published image directly.
ARG HT_IMAGE=ghcr.io/jiangood/ht:latest
FROM ${HT_IMAGE}

COPY data/ht.toml /app/ht.toml
