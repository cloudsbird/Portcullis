# syntax=docker/dockerfile:1

# --- build stage -----------------------------------------------------------
# Build deps: pkg-config + libssl-dev for the ONNX bindings' build script,
# cmake for onnxruntime. The ONNX Runtime itself is downloaded as a static
# archive at build time, which is why the runtime image stays tiny.
FROM rust:1-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
      pkg-config libssl-dev cmake clang \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests

# --features onnx is what pulls in the detector. Drop it for the ~13 MB
# deterministic-only build (see docs/DEPLOYMENT.md).
RUN cargo build --release --features onnx

# --- runtime stage ---------------------------------------------------------
# Only glibc and libstdc++ are needed: `ldd` on the release binary shows
# linux-vdso, libstdc++, libgcc_s, libm and libc, with ONNX Runtime statically
# linked. No Python, no CUDA, no onnxruntime.so to ship.
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10000 --create-home portcullis

COPY --from=builder /src/target/release/portcullis /usr/local/bin/portcullis

USER portcullis
WORKDIR /home/portcullis

# The learned store lives here and must persist across restarts.
ENV PORTCULLIS_STORE=/data/store.json
VOLUME ["/data"]

EXPOSE 8080

# NOTE: publishing this port publishes an UNAUTHENTICATED proxy. Publish to
# 127.0.0.1 only, or terminate auth in front of it. See docs/DEPLOYMENT.md.
ENTRYPOINT ["portcullis"]
CMD ["serve", "--bind", "0.0.0.0:8080"]
