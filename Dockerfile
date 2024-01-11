# Extracts dependencies so we provid from layer caching
# https://www.lpalmieri.com/posts/fast-rust-docker-builds/
FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --bin cheapt

FROM ubuntu:latest AS runtime
ENV RUST_LOG="info,tracing::span=warn,serenity=warn"
COPY --from=builder /app/target/release/cheapt /usr/bin/cheapt
WORKDIR /
ENTRYPOINT ["/usr/bin/cheapt"]
