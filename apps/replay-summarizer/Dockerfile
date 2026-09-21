# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS builder
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends g++ && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build --release --locked --package replay-summarizer && strip target/release/replay-summarizer

FROM node:22.14.0-bookworm AS runtime
ARG PLAYWRIGHT_VERSION=1.58.2
ENV PLAYWRIGHT_BROWSERS_PATH=/opt/browsers
WORKDIR /opt/player
COPY crates/rrweb2video/player/package.json crates/rrweb2video/player/package-lock.json ./
RUN npm ci --ignore-scripts --no-audit --no-fund && \
    npx --yes playwright@${PLAYWRIGHT_VERSION} install --with-deps --only-shell chromium && \
    apt-get update && apt-get install -y --no-install-recommends ffmpeg && \
    rm -rf /var/lib/apt/lists/* && \
    find /opt/browsers -type f \( -name chrome-headless-shell -o -name headless_shell \) -exec ln -sf '{}' /usr/local/bin/chromium-headless-real \; && \
    test -x /usr/local/bin/chromium-headless-real
COPY apps/replay-summarizer/chromium.sh /usr/local/bin/chromium-headless
# Set permissions explicitly so builders need not support COPY --chmod.
RUN chmod 755 /usr/local/bin/chromium-headless
COPY --from=builder /app/target/release/replay-summarizer /usr/local/bin/replay-summarizer
USER node
WORKDIR /tmp
RUN /usr/local/bin/chromium-headless --version
ENTRYPOINT ["/usr/local/bin/replay-summarizer"]
