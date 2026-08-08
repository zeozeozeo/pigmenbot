# syntax=docker/dockerfile:1.7

FROM rust:nightly-bookworm AS builder

WORKDIR /usr/src/pigmenfarm
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets

RUN cargo build --release --locked

FROM debian:bookworm-slim

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install --no-install-recommends -y ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 --shell /usr/sbin/nologin pigmenfarm \
    && mkdir /data \
    && chown pigmenfarm:pigmenfarm /data

COPY --from=builder /usr/src/pigmenfarm/target/release/pigmenfarm /usr/local/bin/pigmenfarm

WORKDIR /data
USER pigmenfarm
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/pigmenfarm"]
