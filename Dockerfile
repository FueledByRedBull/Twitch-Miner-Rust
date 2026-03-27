FROM rust:1.94-bookworm AS build
WORKDIR /workspace
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY tests ./tests
COPY . .
RUN cargo build --release -p tm-app

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /workspace/target/release/tm-app /usr/local/bin/twitch-miner
ENV TCPM_DATA_DIR=/data
ENV TCPM_CONFIG=/data/config.json
STOPSIGNAL SIGTERM
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/twitch-miner"]
