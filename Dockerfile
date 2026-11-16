# syntax=docker/dockerfile:1
FROM rust:1-slim-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY migrations ./migrations
COPY src ./src
RUN cargo build --locked --release && \
    install -D target/release/endless-drawing-server /out/endless-drawing-server

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /out/endless-drawing-server /app/endless-drawing-server
# CI checks out the separate frontend repository and builds dist before docker build.
COPY frontend-source/dist/ /app/static/
ENV BIND_ADDR=0.0.0.0:3000 STATIC_DIR=/app/static RUST_LOG=info
USER 10001:10001
EXPOSE 3000
HEALTHCHECK --interval=5s --timeout=4s --start-period=10s --retries=12 \
    CMD curl --fail --silent --show-error --max-time 3 http://127.0.0.1:3000/readyz || exit 1
ENTRYPOINT ["/app/endless-drawing-server"]
