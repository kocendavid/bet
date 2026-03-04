FROM rust:1.90-bookworm AS builder
WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

# Cache dependency resolution for workspace members used by matcher-service.
COPY Cargo.toml Cargo.lock ./
COPY matcher-service/Cargo.toml matcher-service/Cargo.toml
COPY ledger-service/Cargo.toml ledger-service/Cargo.toml
COPY proto proto
COPY matcher-service matcher-service
COPY ledger-service ledger-service

RUN cargo build --release -p matcher-service

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/matcher-service /usr/local/bin/matcher-service

# Railway sets PORT dynamically. Default to 8080 for local container runs.
ENV PORT=8080
ENV MATCHER_LISTEN_ADDR=0.0.0.0:8080
ENV MATCHER_LEDGER_MODE=accept_all
ENV MATCHER_DATA_DIR=/app/matcher-data

EXPOSE 8080

CMD ["sh", "-lc", "MATCHER_LISTEN_ADDR=0.0.0.0:${PORT} matcher-service"]
