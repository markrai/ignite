# Ignite Server — Linux runtime image (glibc, host-network Compose/Portainer).
#
# Build context must be a staged directory produced by build-image.sh containing:
#   scribe-server, web/, entrypoint.sh, config.toml
#
# Supported architecture for this packaging path: linux/amd64 (validated).
# Do not assume ARM works until a matching Linux bundle is built and tested.
#
# Base matches the Ubuntu 24.04 glibc used by the reference Linux bundle builds.

FROM ubuntu:24.04

LABEL org.opencontainers.image.title="Ignite Server" \
      org.opencontainers.image.description="Trusted-LAN Ignite Server (host-network Compose/Portainer)" \
      org.opencontainers.image.source="https://github.com/markrai/ignite"

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
         ca-certificates \
         curl \
         libssl3t64 \
         util-linux \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 19187 scribe \
    && useradd --uid 19187 --gid 19187 \
         --home-dir /var/lib/scribe-server --no-create-home \
         --shell /usr/sbin/nologin \
         scribe \
    && mkdir -p /var/lib/scribe-server /etc/scribe-server /usr/share/scribe-server \
    && chown 19187:19187 /var/lib/scribe-server \
    && chmod 700 /var/lib/scribe-server

# Application payload (read-only at runtime; not mixed with state).
COPY scribe-server /usr/local/bin/scribe-server
COPY web/ /usr/share/scribe-server/web/
COPY entrypoint.sh /usr/local/bin/docker-entrypoint.sh
COPY config.toml /etc/scribe-server/config.toml

RUN chmod 0755 /usr/local/bin/scribe-server /usr/local/bin/docker-entrypoint.sh \
    && chmod -R a+rX /usr/share/scribe-server/web \
    && chmod 0644 /etc/scribe-server/config.toml \
    && test -x /usr/local/bin/scribe-server \
    && /usr/local/bin/scribe-server --version \
    && test -f /usr/share/scribe-server/web/index.html \
    && test -f /usr/share/scribe-server/web/manifest.webmanifest

# Image config USER is unset (root). Entrypoint may start as root to chown the state
# volume, then exec setpriv → UID/GID 19187 with inheritable/ambient capabilities cleared.
# PID 1 after drop is non-root; do not claim the container never runs any root code.
EXPOSE 8787

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8787/control/v1/status || exit 1

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["serve", "--config", "/etc/scribe-server/config.toml"]
