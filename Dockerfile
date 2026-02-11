FROM rust:1.93-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./

RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --release && \
    rm -rf src

COPY src ./src

RUN cargo build --release


FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

RUN useradd --system --create-home --uid 10001 app

WORKDIR /app

COPY --from=builder /app/target/release/data-collector ./data-collector
COPY regexes.yaml ./regexes.yaml

RUN mkdir -p /home/app/data && \
    chown -R app:app /home/app/data

USER app

ENV PORT=3000
ENV BACKUP_DB_PATH=/home/app/data/backup.db
ENV UA_REGEXES_PATH=/app/regexes.yaml

EXPOSE 3000

CMD ["./data-collector"]
