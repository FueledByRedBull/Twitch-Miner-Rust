# syntax=docker/dockerfile:1.7@sha256:a57df69d0ea827fb7266491f2813635de6f17269be881f696fbfdf2d83dda33e
FROM rust:1.94.0-bookworm@sha256:365468470075493dc4583f47387001854321c5a8583ea9604b297e67f01c5a4f AS chef
WORKDIR /workspace
RUN printf '%s\n' \
        'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/20250101T000000Z bookworm main' \
        > /etc/apt/sources.list \
    && rm -f /etc/apt/sources.list.d/debian.sources \
    && apt-get -o Acquire::Check-Valid-Until=false update \
    && apt-get install -y --no-install-recommends musl-tools=1.2.3-1 \
    && rm -rf /var/lib/apt/lists/*
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/tmp/cargo-install-target \
    CARGO_TARGET_DIR=/tmp/cargo-install-target cargo install cargo-chef --version 0.1.77 --locked

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
RUN mkdir -p crates/tm-app/src crates/tm-auth/src crates/tm-config/src crates/tm-domain/src crates/tm-irc/src crates/tm-observability/src crates/tm-pubsub/src crates/tm-runtime/src crates/tm-twitch/src \
    && printf 'fn main() {}\n' > crates/tm-app/src/main.rs \
    && printf '\n' > crates/tm-auth/src/lib.rs \
    && printf '\n' > crates/tm-config/src/lib.rs \
    && printf '\n' > crates/tm-domain/src/lib.rs \
    && printf '\n' > crates/tm-irc/src/lib.rs \
    && printf '\n' > crates/tm-observability/src/lib.rs \
    && printf '\n' > crates/tm-pubsub/src/lib.rs \
    && printf '\n' > crates/tm-runtime/src/lib.rs \
    && printf '\n' > crates/tm-twitch/src/lib.rs
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
ARG TARGETARCH
ARG TARGETVARIANT
ENV CARGO_INCREMENTAL=0
ENV RUSTFLAGS=--remap-path-prefix=/workspace=.
COPY --from=planner /workspace/recipe.json recipe.json
RUN case "${TARGETARCH}:${TARGETVARIANT}" in \
        "amd64:") rust_target="x86_64-unknown-linux-musl" ;; \
        "arm64:") rust_target="aarch64-unknown-linux-musl" ;; \
        *) echo "unsupported Docker platform: ${TARGETARCH}/${TARGETVARIANT}" >&2; exit 1 ;; \
    esac \
    && rustup target add "${rust_target}" \
    && cargo chef cook --locked --release --target "${rust_target}" --recipe-path recipe.json
ARG BUILD_REVISION=unknown
ARG SOURCE_DATE_EPOCH=0
ENV BUILD_REVISION=${BUILD_REVISION}
ENV SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH}
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN case "${TARGETARCH}:${TARGETVARIANT}" in \
        "amd64:") rust_target="x86_64-unknown-linux-musl" ;; \
        "arm64:") rust_target="aarch64-unknown-linux-musl" ;; \
        *) echo "unsupported Docker platform: ${TARGETARCH}/${TARGETVARIANT}" >&2; exit 1 ;; \
    esac \
    && cargo build --locked --release --target "${rust_target}" -p tm-app \
    && install -D "/workspace/target/${rust_target}/release/tm-app" /workspace/bin/twitch-miner

FROM scratch AS runtime
COPY --from=build /workspace/bin/twitch-miner /twitch-miner
ENV TCPM_DATA_DIR=/data
ENV TCPM_CONFIG=/data/config.json
WORKDIR /data
USER 65532:65532
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=60s --timeout=5s --start-period=90s --retries=3 CMD ["/twitch-miner", "--health"]
VOLUME ["/data"]
ENTRYPOINT ["/twitch-miner"]
