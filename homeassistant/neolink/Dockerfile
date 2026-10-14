# Neolink Home Assistant add-on.
# Base is the image .github/workflows/docker-ghcr.yml publishes for this fork (amd64),
# pinned to a release tag (v*). Bump BUILD_FROM and config.yaml `version` together.
ARG BUILD_FROM=ghcr.io/npolanosky/neolink:v0.7.0-ha.7
FROM ${BUILD_FROM}

# Supervisor writes /data/options.json as root
USER root

RUN apt-get update \
    && apt-get install -y --no-install-recommends jq \
    && rm -rf /var/lib/apt/lists/*

COPY run.sh /run.sh
RUN chmod +x /run.sh

ENTRYPOINT ["/run.sh"]
CMD []
