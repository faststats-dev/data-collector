# syntax=docker/dockerfile:1

# Model tooling and artifacts have their own cache chain, independent of Rust.
FROM python:3.12-slim-bookworm AS model-deps
WORKDIR /tools
COPY apps/error-embedder/model/requirements.txt ./
# Use CPU PyTorch wheels; the default Linux wheels pull large CUDA dependencies.
RUN --mount=type=cache,target=/root/.cache/pip,sharing=locked \
    pip install --index-url https://download.pytorch.org/whl/cpu torch==2.8.0 \
    && pip install -r requirements.txt
RUN cp /usr/local/lib/python3.12/site-packages/onnxruntime/capi/libonnxruntime.so.1.23.2 /libonnxruntime.so

FROM model-deps AS model-download
COPY apps/error-embedder/model/download-model.py ./
# Retain verified downloads even when this layer is invalidated by tool changes.
RUN --mount=type=cache,id=error-embedder-checkpoint,target=/checkpoint-cache,sharing=locked \
    python download-model.py --directory /checkpoint-cache \
    && mkdir /model \
    && cp -r /checkpoint-cache/model.safetensors /checkpoint-cache/tokenizer.json /checkpoint-cache/jina_source /model/

FROM model-download AS model
COPY apps/error-embedder/model/export-model.py ./
# Export includes full-vector/threshold validation; failures stop the image build.
RUN --network=none python export-model.py --directory /model

# Optional target for inspecting/exporting the exact runtime artifacts.
FROM scratch AS model-artifacts
COPY --from=model /model/model-fp16.onnx /model/fp16.json /model/tokenizer.json /models/

FROM rust:1.98-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY apps ./apps
COPY crates ./crates
ARG TARGETARCH
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=error-embedder-rust-${TARGETARCH},target=/app/target,sharing=locked \
    cargo build --release --locked -p error-embedder \
    && cp target/release/error-embedder /error-embedder && strip /error-embedder

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 libstdc++6 libgomp1 && rm -rf /var/lib/apt/lists/*
COPY --from=build /error-embedder /usr/local/bin/error-embedder
COPY --from=model-deps /libonnxruntime.so /usr/local/lib/libonnxruntime.so
COPY --from=model-artifacts --chown=65532:65532 --chmod=0444 /models/model-fp16.onnx /models/fp16.json /models/tokenizer.json /models/
RUN chmod 0555 /models
ENV EMBED_MODEL_DIR=/models EMBED_PRECISION=fp16 EMBED_THREADS=2 RUST_LOG=info ORT_DYLIB_PATH=/usr/local/lib/libonnxruntime.so
USER 65532:65532
ENTRYPOINT ["error-embedder"]
CMD ["consume"]
