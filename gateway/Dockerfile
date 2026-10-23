# The gateway.
#
# Two stages: a toolchain that compiles, and a runtime that does not. The
# image that faces the internet should not contain a compiler, a package
# manager or a shell's worth of tooling it will never use.
#
# The engine crates are copied in as well because the gateway depends on them
# by path — the workspace is the unit that builds, not this directory.

FROM rust:1.83-slim-bookworm AS build

# `ring` needs a C toolchain; `rustls` needs nothing else.
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Manifests first, so a change to source does not re-download the registry.
COPY engine/Cargo.toml engine/Cargo.lock engine/
COPY engine/crates engine/crates
COPY gateway/Cargo.toml gateway/Cargo.lock gateway/

# A throwaway main, so dependencies compile into a layer of their own and
# stay cached while the gateway itself changes.
RUN mkdir -p gateway/src && echo 'fn main() {}' > gateway/src/main.rs
WORKDIR /src/gateway
RUN cargo build --release --locked && rm -rf src target/release/deps/gateway*

COPY gateway/src src
RUN cargo build --release --locked

# ── The image that runs ──
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home --home-dir /app feraro

COPY --from=build /src/gateway/target/release/gateway /usr/local/bin/gateway

USER feraro
WORKDIR /app

# The journal is the database, so this is the one directory that matters.
# Mount a volume here or the platform forgets everything on redeploy.
VOLUME ["/app/journal"]

ENV FERARO_BIND=0.0.0.0:8081 \
    FERARO_HEALTH_BIND=0.0.0.0:8082 \
    FERARO_JOURNAL=/app/journal \
    FERARO_USERS=/app/journal/users.json

EXPOSE 8081 8082

# The proxy and the supervisor both read this. It answers 503 once prices
# stop arriving, because a gateway that is running but unpriced is an
# outage, not a healthy process.
HEALTHCHECK --interval=20s --timeout=4s --start-period=40s --retries=3 \
    CMD ["/usr/local/bin/gateway", "--healthcheck"]

ENTRYPOINT ["/usr/local/bin/gateway"]
