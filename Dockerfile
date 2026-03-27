# syntax=docker/dockerfile:1.7
FROM rust:1.94-bookworm AS chef
WORKDIR /workspace
RUN cargo install cargo-chef --locked

FROM chef AS planner
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/tm-app/Cargo.toml crates/tm-app/Cargo.toml
COPY crates/tm-auth/Cargo.toml crates/tm-auth/Cargo.toml
COPY crates/tm-config/Cargo.toml crates/tm-config/Cargo.toml
COPY crates/tm-domain/Cargo.toml crates/tm-domain/Cargo.toml
COPY crates/tm-irc/Cargo.toml crates/tm-irc/Cargo.toml
COPY crates/tm-observability/Cargo.toml crates/tm-observability/Cargo.toml
COPY crates/tm-pubsub/Cargo.toml crates/tm-pubsub/Cargo.toml
COPY crates/tm-runtime/Cargo.toml crates/tm-runtime/Cargo.toml
COPY crates/tm-twitch/Cargo.toml crates/tm-twitch/Cargo.toml
COPY crates/tm-updater/Cargo.toml crates/tm-updater/Cargo.toml
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=planner /workspace/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/workspace/target \
    cargo chef cook --release --recipe-path recipe.json
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/workspace/target \
    cargo build --locked --release -p tm-app

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
