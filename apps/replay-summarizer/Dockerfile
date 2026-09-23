# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS builder
WORKDIR /app
COPY . .
RUN cargo build --release --locked --package replay-summarizer --bin replay-summarizer && strip target/release/replay-summarizer

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* && useradd --uid 1000 --create-home worker
COPY --from=builder /app/target/release/replay-summarizer /usr/local/bin/replay-summarizer
USER worker
WORKDIR /tmp
ENTRYPOINT ["/usr/local/bin/replay-summarizer"]
