# Step 1 - Ledger MVP (Reservations, Idempotency, Fills)

## Objective

Implement canonical money and position accounting with strict non-negative guarantees and idempotent write paths.

## Scope

- Postgres schema and migrations for balances, reservations, positions, fill ledger, idempotency tables.
- Ledger gRPC API for reserve/adjust/release/apply fill/query.
- Atomic fill application with taker fee and reduce-long-first behavior.
- Docker Compose for local Ledger + Postgres startup and integration test dependencies.

## Technical Design

### Schema

- `wallets(user_id PK, available_czk BIGINT NOT NULL, reserved_czk BIGINT NOT NULL, updated_at)`
- `reservations(reservation_id PK, user_id, order_id, kind, amount_czk, consumed_czk, status, created_at, updated_at)`
- `positions(user_id, market_id, outcome_id, long_qty BIGINT, short_qty BIGINT, PRIMARY KEY(user_id, market_id, outcome_id))`
- `fills(fill_id PK, maker_user_id, taker_user_id, order_ids, qty, price_czk, notional_czk, fee_czk, created_at)`
- `processed_commands(command_id PK, operation_type, created_at)`

### Transaction Rules

- Use `SERIALIZABLE` isolation for all reserve/adjust/release/apply fill write transactions.
- Lock wallet rows by `user_id` before mutation using `SELECT ... FOR UPDATE`.
- Enforce invariant after every mutation:
  - `available_czk >= 0`
  - `reserved_czk >= 0`

### gRPC Methods

- `ReserveForOrder(command_id, user_id, order_id, kind, amount_czk)`
- `AdjustReservation(command_id, reservation_id, delta_czk)`
- `ReleaseReservation(command_id, reservation_id)`
- `ApplyFill(fill_id, maker, taker, qty, price_czk, taker_side, fee_bps, order_refs)`
- `GetBalances(user_id)`
- `GetPositions(user_id)`

### Fill Logic

- Compute notional: `qty * price_czk`.
- Compute taker fee: `ceil(notional * fee_bps / 10000)`.
- Buyer: debit notional + fee.
- Seller: credit notional.
- Position update:
  - buy => increment `long_qty`
  - sell => reduce `long_qty` first, remainder increments `short_qty`

## Implementation Tasks

1. Create SQL migrations and schema constraints.
2. Define protobufs and generate Rust stubs.
3. Implement repository layer with typed queries and transaction wrappers.
4. Implement service logic for each gRPC method.
5. Add idempotency checks using unique constraints and conflict handling.
6. Add audit log emission hooks (in-process event structs).
7. Add `docker-compose.yml` for local Postgres + Ledger service, with documented commands for `up`, `down`, and test execution.

## Test Plan

- Unit tests for:
  - integer conversion/rounding,
  - fee rounding,
  - reservation math,
  - reduce-long-first position transitions.
- Transaction concurrency tests (100+ parallel reserve requests on same wallet).
- Idempotency tests (`ApplyFill` duplicate `fill_id` must be no-op).
- Property tests:
  - balances never negative,
  - total CZK conservation minus collected fees.

## Acceptance Criteria

- No negative balances in stress tests.
- Duplicate command/fill requests are idempotent.
- Test suite passes deterministically across repeated runs.

## Deliverables

- migrations,
- ledger protobuf contracts,
- ledger service implementation,
- Docker Compose setup and run instructions,
- test suite and test report.
