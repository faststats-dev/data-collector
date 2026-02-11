FROM rust:1.92-slim-bookworm AS builder
WORKDIR /app
COPY . .
RUN cargo build --release --bin data-collector

FROM gcr.io/distroless/cc-debian12:nonroot
COPY regexes.yaml ./regexes.yaml
COPY --from=builder /app/target/release/data-collector /usr/local/bin/data-collector
USER nonroot:nonroot

CMD ["/usr/local/bin/data-collector"]
