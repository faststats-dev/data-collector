FROM rust:1-bookworm AS builder

WORKDIR /app
RUN apt-get update \
    && apt-get install -y --no-install-recommends clang cmake g++ libclang-dev libsasl2-dev libssl-dev libzstd-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY . .
RUN cargo build --release --locked --package replay-consumer --bin replay-consumer \
    && strip /app/target/release/replay-consumer

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libsasl2-2 libssl3 libzstd1 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/replay-consumer /usr/local/bin/replay-consumer

USER 65532:65532
ENTRYPOINT ["/usr/local/bin/replay-consumer"]
