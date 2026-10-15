FROM rust:1.99.0-bookworm AS build

RUN apt-get update \
    && apt-get install --no-install-recommends -y cmake pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY web ./web
RUN cargo build --locked --release

FROM debian:bookworm-slim

ARG BUILD_VERSION=dev
LABEL org.opencontainers.image.source="https://github.com/jessfraz/ha-packages" \
      org.opencontainers.image.licenses="MIT" \
      io.hass.version="${BUILD_VERSION}" \
      io.hass.type="app" \
      io.hass.arch="aarch64|amd64"

RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates gosu \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 shipments \
    && useradd --uid 10001 --gid shipments --no-create-home --shell /usr/sbin/nologin shipments \
    && install -d -o shipments -g shipments /data

COPY --from=build /src/target/release/shipments /usr/local/bin/shipments
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

ENV SHIPMENTS_BIND=0.0.0.0:8080 \
    SHIPMENTS_DATABASE=/data/shipments.sqlite3

WORKDIR /data
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
