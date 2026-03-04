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

## Path Analysis (Core Flows)

### Happy Paths

- `PlaceOrder` passes validation and reservation, then fully matches and emits deterministic fill/order events.
- `PlaceOrder` passes validation and reservation, partially matches, then rests remaining quantity at correct queue position.
- `CancelOrder` for active open order removes it and releases reservation.
- Restart recovery loads snapshot + replays log suffix to the same final state hash.

### Bad/Failure Paths

- Reservation rejection from Ledger causes `OrderRejected` with no order-book mutation.
- Duplicate `command_id` is handled idempotently (no duplicate order/fills).
- Cancel for non-existent/already-finalized order returns deterministic reject/no-op.
- Transient Ledger RPC failures exhaust retries and reject command without partial book mutation.

### Edge Cases

- Self-trade prevention skips maker self-orders but continues deterministic walk across subsequent price levels.
- IOC order with partial cross cancels exact remainder and releases only remainder reservation.
- Two orders at same price preserve strict FIFO under replay and restart.
- Snapshot boundary (command exactly at snapshot cutover) replays without double-apply or missed events.

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
