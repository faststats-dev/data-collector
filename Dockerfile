FROM rust:1-bookworm AS chef
RUN rustup target add x86_64-unknown-linux-musl
RUN cargo install cargo-chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
RUN apt-get update && apt-get install -y musl-tools pkg-config \
    && rm -rf /var/lib/apt/lists/*
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook \
    --release \
    --target x86_64-unknown-linux-musl \
    --recipe-path recipe.json
COPY . .
RUN cargo build \
    --release \
    --target x86_64-unknown-linux-musl \
    --bin data-collector

FROM gcr.io/distroless/static-debian12 AS runtime
WORKDIR /app
COPY regexes.yaml ./regexes.yaml
COPY --from=builder \
    /app/target/x86_64-unknown-linux-musl/release/data-collector \
    /usr/local/bin/data-collector
USER nonroot:nonroot

ENV PORT=8080
EXPOSE $PORT

ENTRYPOINT ["/usr/local/bin/data-collector"]
