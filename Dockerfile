FROM rust:1.92-slim AS chef
WORKDIR /app
RUN cargo install cargo-chef

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
COPY regexes.yaml ./regexes.yaml
RUN cargo build --release --bin data-collector

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /app/target/release/data-collector /usr/local/bin/data-collector
USER nonroot:nonroot

CMD ["/usr/local/bin/data-collector"]
