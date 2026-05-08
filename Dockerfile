FROM rust:1-slim-bookworm AS build

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git nano openssh-client vim \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --create-home --home-dir /home/refray --shell /usr/sbin/nologin refray \
    && mkdir -p /data \
    && chown -R refray:refray /data

COPY --from=build /app/target/release/refray /usr/local/bin/refray

USER refray
WORKDIR /data
ENV XDG_CONFIG_HOME=/data/config \
    XDG_CACHE_HOME=/data/cache

EXPOSE 8787
ENTRYPOINT ["refray"]
CMD ["serve", "--listen", "0.0.0.0:8787"]
