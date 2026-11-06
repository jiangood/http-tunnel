# Optional: bake the config into an image instead of mounting ./data.
# The docker.sock itself is still mounted at run time; never bake it into an image.
ARG HT_IMAGE=ghcr.io/jiangood/ht:latest
FROM ${HT_IMAGE}

COPY data/ht.toml /app/ht.toml
