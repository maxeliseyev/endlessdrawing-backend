# syntax=docker/dockerfile:1
# Dependencies are compiled in their own layer (cargo-chef), keyed only by
# Cargo.toml/Cargo.lock: a source-only change reuses it from the CI layer cache
# and recompiles just this crate.
FROM rust:1-slim-bookworm AS chef
RUN cargo install cargo-chef --locked --version 0.1.78
WORKDIR /build

FROM chef AS planner
COPY Cargo.toml Cargo.lock build.rs ./
COPY migrations ./migrations
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --locked --release --recipe-path recipe.json
COPY Cargo.toml Cargo.lock build.rs ./
COPY migrations ./migrations
COPY src ./src
RUN cargo build --locked --release && \
    install -D target/release/endless-drawing-server /out/endless-drawing-server && \
    install -D target/release/canvas-admin /out/canvas-admin && \
    install -D target/release/canvas-migrate /out/canvas-migrate

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
LABEL io.endlessdrawing.db-roles-version="1"
COPY --from=builder /out/endless-drawing-server /app/endless-drawing-server
COPY --from=builder /out/canvas-admin /app/canvas-admin
COPY --from=builder /out/canvas-migrate /app/canvas-migrate
# CI checks out the separate frontend repository and builds dist before docker build.
COPY frontend-source/dist/ /app/static/
ENV BIND_ADDR=0.0.0.0:3000 STATIC_DIR=/app/static RUST_LOG=info
USER 10001:10001
EXPOSE 3000
HEALTHCHECK --interval=5s --timeout=4s --start-period=10s --retries=12 \
    CMD curl --fail --silent --show-error --max-time 3 http://127.0.0.1:3000/readyz || exit 1
ENTRYPOINT ["/app/endless-drawing-server"]
