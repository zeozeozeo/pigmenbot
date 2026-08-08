# syntax=docker/dockerfile:1.7

FROM rustlang/rust:nightly-bookworm AS builder

WORKDIR /usr/src/pigmenfarm
COPY Cargo.toml Cargo.lock ./

# Keep dependency compilation in a source-independent layer. The GitHub
# Actions BuildKit cache can reuse this layer when only the bot changes.
RUN mkdir src \
    && printf 'fn main() {}\n' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
COPY assets ./assets

RUN touch src/main.rs \
    && cargo build --release --locked \
    && cp target/release/pigmenfarm /usr/src/pigmenfarm/pigmenfarm

FROM debian:bookworm-slim

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install --no-install-recommends -y ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 --shell /usr/sbin/nologin pigmenfarm \
    && mkdir /data \
    && chown pigmenfarm:pigmenfarm /data

COPY --from=builder /usr/src/pigmenfarm/pigmenfarm /usr/local/bin/pigmenfarm

WORKDIR /data
USER pigmenfarm
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/pigmenfarm"]
