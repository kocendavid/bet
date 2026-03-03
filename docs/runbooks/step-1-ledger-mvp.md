# Step 1 Runbook: Ledger MVP

## Prerequisites

- Docker and Docker Compose
- Rust toolchain (for local test execution)

## Start services

```bash
docker compose up --build -d
```

## Stop services

```bash
docker compose down
```

## Stop services and remove data

```bash
docker compose down -v
```

## Run tests

```bash
cargo test
```

## Notes

- `ledger-service` applies SQL migrations on startup from `ledger-service/migrations`.
- gRPC endpoint binds to `0.0.0.0:50051`.
- Monetary values are integer-only CZK units.
