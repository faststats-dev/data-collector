FROM rust:1-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config \
    && rm -rf /var/lib/apt/lists/*
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook \
    --release \
    --recipe-path recipe.json
COPY . .
RUN cargo build \
    --release \
    --package collector \
    --bin collector \
    && strip /app/target/release/collector

FROM gcr.io/distroless/cc-debian12 AS runtime
WORKDIR /app
COPY --from=builder \
    /app/target/release/collector \
    /usr/local/bin/collector
USER nonroot:nonroot

ENV PORT=8080
EXPOSE $PORT

ENTRYPOINT ["/usr/local/bin/collector"]
