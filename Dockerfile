# syntax=docker/dockerfile:1.7
FROM rust:1.94.0-bookworm AS chef
WORKDIR /workspace
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/*
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/tmp/cargo-install-target \
    CARGO_TARGET_DIR=/tmp/cargo-install-target cargo install cargo-chef --locked

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
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
COPY tests/contract/Cargo.toml tests/contract/Cargo.toml
COPY tests/integration/Cargo.toml tests/integration/Cargo.toml
RUN mkdir -p crates/tm-app/src crates/tm-auth/src crates/tm-config/src crates/tm-domain/src crates/tm-irc/src crates/tm-observability/src crates/tm-pubsub/src crates/tm-runtime/src crates/tm-twitch/src crates/tm-updater/src tests/contract/src tests/integration/src \
    && printf 'fn main() {}\n' > crates/tm-app/src/main.rs \
    && printf '\n' > crates/tm-auth/src/lib.rs \
    && printf '\n' > crates/tm-config/src/lib.rs \
    && printf '\n' > crates/tm-domain/src/lib.rs \
    && printf '\n' > crates/tm-irc/src/lib.rs \
    && printf '\n' > crates/tm-observability/src/lib.rs \
    && printf '\n' > crates/tm-pubsub/src/lib.rs \
    && printf '\n' > crates/tm-runtime/src/lib.rs \
    && printf '\n' > crates/tm-twitch/src/lib.rs \
    && printf '\n' > crates/tm-updater/src/lib.rs \
    && printf '\n' > tests/contract/src/lib.rs \
    && printf '\n' > tests/integration/src/lib.rs
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
ARG TARGETARCH
ARG TARGETVARIANT
COPY --from=planner /workspace/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/workspace/target \
    case "${TARGETARCH}:${TARGETVARIANT}" in \
        "amd64:") rust_target="x86_64-unknown-linux-musl" ;; \
        "arm64:") rust_target="aarch64-unknown-linux-musl" ;; \
        "arm:v7") rust_target="armv7-unknown-linux-musleabihf" ;; \
        *) echo "unsupported Docker platform: ${TARGETARCH}/${TARGETVARIANT}" >&2; exit 1 ;; \
    esac \
    && rustup target add "${rust_target}" \
    && cargo chef cook --release --target "${rust_target}" --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY tests/contract/Cargo.toml tests/contract/Cargo.toml
COPY tests/integration/Cargo.toml tests/integration/Cargo.toml
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/workspace/target \
    mkdir -p tests/contract/src tests/integration/src \
    && printf '\n' > tests/contract/src/lib.rs \
    && printf '\n' > tests/integration/src/lib.rs \
    && case "${TARGETARCH}:${TARGETVARIANT}" in \
        "amd64:") rust_target="x86_64-unknown-linux-musl" ;; \
        "arm64:") rust_target="aarch64-unknown-linux-musl" ;; \
        "arm:v7") rust_target="armv7-unknown-linux-musleabihf" ;; \
        *) echo "unsupported Docker platform: ${TARGETARCH}/${TARGETVARIANT}" >&2; exit 1 ;; \
    esac \
    && cargo build --locked --release --target "${rust_target}" -p tm-app \
    && install -D "/workspace/target/${rust_target}/release/tm-app" /workspace/bin/twitch-miner

FROM scratch
COPY --from=build /workspace/bin/twitch-miner /twitch-miner
ENV TCPM_DATA_DIR=/data
ENV TCPM_CONFIG=/data/config.json
WORKDIR /data
USER 65532:65532
STOPSIGNAL SIGTERM
VOLUME ["/data"]
ENTRYPOINT ["/twitch-miner"]
