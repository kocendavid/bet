# Step 2 - Deterministic Matcher MVP (Single Book + Sync Reservations)

## Objective

Implement a deterministic single-outcome matching engine that performs synchronous reservation checks with the Ledger before matching/resting orders.

## Scope

- In-memory CLOB for one `(market_id, outcome_id)`.
- Commands: place/cancel.
- Deterministic matching with price-time priority.
- Append-only command log and snapshot recovery.
- Basic HTTP gateway for order submission and state query.

## Technical Design

### State Machine Inputs

- `PlaceOrder(command_id, user_id, order_id, side, type, limit_price, qty)`
- `CancelOrder(command_id, user_id, order_id)`

### State Machine Outputs

- `OrderAccepted`, `OrderRejected(reason)`, `OrderRested`, `OrderCanceled`
- `TradeExecuted(fill_id, maker_order_id, taker_order_id, price, qty)`
- `OrderPartial`, `OrderFilled`

### Matching Rules

- FIFO within same price level.
- Buy crosses asks where `ask <= limit`.
- Sell crosses bids where `bid >= limit`.
- IOC remainder canceled.
- Self-trade prevention policy: skip self maker order and continue deterministic walk.

### Ledger Coordination

- On place:
  1. Compute worst-case reservation.
  2. Call `ReserveForOrder` on Ledger synchronously.
  3. Continue only on success.
- On cancel/unfilled remainder:
  - call `ReleaseReservation`.
- On each trade:
  - emit fill intent and call `ApplyFill`.

### Persistence

- Command log file format: length-prefixed protobuf records with sequence number.
- Snapshot includes:
  - all price levels,
  - open order map,
  - sequence counters.
- Recovery:
  - load latest snapshot,
  - replay log suffix.

## Implementation Tasks

1. Implement order book data structures (bid/ask trees + per-level FIFO queue).
2. Implement deterministic sequence allocator per book.
3. Implement matcher command processor and event emitter.
4. Implement ledger RPC client adapter with retry policy for transient transport errors.
5. Implement command log writer and snapshot serializer.
6. Implement HTTP gateway endpoints for place/cancel/book query.

## Test Plan

- Golden tests for fixed command sequences.
- Replay determinism tests using final state hash.
- Matching correctness tests for partial fills and multi-level crossing.
- IOC behavior tests (fully filled vs partial + canceled remainder).
- Self-trade skip determinism tests.

## Acceptance Criteria

- Replaying identical command log yields identical final state hash.
- All matching invariants pass in unit/integration tests.
- Reservation failures reject orders without mutating book state.

## Deliverables

- matcher service,
- command log + snapshot modules,
- gateway MVP endpoints,
- deterministic test fixtures.
