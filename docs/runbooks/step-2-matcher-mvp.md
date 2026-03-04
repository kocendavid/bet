# Step 2 Runbook: Deterministic Matcher MVP

## Prerequisites

- Step 1 Ledger running (`ledger-service` gRPC on `:50051`)
- Rust toolchain

## Run matcher

```bash
MATCHER_LISTEN_ADDR=0.0.0.0:8080 \
LEDGER_GRPC_ADDR=http://127.0.0.1:50051 \
MATCHER_DATA_DIR=./matcher-data \
cargo run -p matcher-service
```

Local test mode (skips real ledger reservation/fill calls; useful for synthetic load):

```bash
MATCHER_LISTEN_ADDR=0.0.0.0:8080 \
MATCHER_LEDGER_MODE=accept_all \
MATCHER_DATA_DIR=./matcher-data \
cargo run -p matcher-service
```

## Example API calls

Place order:

```bash
curl -sS -X POST http://127.0.0.1:8080/orders \
  -H 'content-type: application/json' \
  -d '{
    "command_id":"cmd-1",
    "market_id":"market-1",
    "outcome_id":"outcome-1",
    "user_id":"user-a",
    "order_id":"ord-1",
    "side":"buy",
    "order_type":"limit",
    "limit_price":6000,
    "qty":10
  }'
```

Cancel order:

```bash
curl -sS -X DELETE http://127.0.0.1:8080/orders/ord-1 \
  -H 'content-type: application/json' \
  -d '{"command_id":"cmd-2","market_id":"market-1","outcome_id":"outcome-1","user_id":"user-a"}'
```

Query book:

```bash
curl -sS http://127.0.0.1:8080/books/market-1/outcome-1
```

## Dashboard + Fake Traffic

Open the built-in dashboard:

```bash
open http://127.0.0.1:8080/dashboard
```

Start synthetic order flow:

```bash
BASE_URL=http://127.0.0.1:8080 \
MARKET_ID=market-demo \
OUTCOME_ID=yes \
USERS=20 \
RATE_PER_SEC=8 \
./scripts/fake_traffic.sh
```

Environment options:

- `CANCEL_PCT` default `25`
- `BOOTSTRAP_MARKET` default `1` (creates market via `/admin/markets`)
- `ADMIN_TOKEN` default `admin-token`

## Recovery notes

- Command log: `MATCHER_DATA_DIR/commands.log` (length-prefixed protobuf records)
- Snapshot: `MATCHER_DATA_DIR/snapshot.pb`
- On restart, matcher loads snapshot then replays log suffix.

## Run tests

```bash
cargo test -p matcher-service
```
