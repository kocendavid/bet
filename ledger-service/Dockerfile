FROM rust:1.90-bookworm AS builder
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY ledger-service/Cargo.toml ledger-service/Cargo.toml
COPY proto proto
COPY ledger-service ledger-service

RUN cargo build --release -p ledger-service

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/ledger-service /usr/local/bin/ledger-service
EXPOSE 50051
CMD ["ledger-service"]
