# Prediction Market Platform Spec

## Target Stack
- Rust services
- Kafka for downstream distribution
- WebSockets for market data and user notifications
- Postgres for canonical storage (ledger, markets, resolution)
- Object storage (S3/MinIO) for snapshots/log segments if desired
- Matching shards are single-writer deterministic state machines with append-only command log

## Repository Layout and Shared Conventions

### Workspace
- `crates/`
- `common/` (ids, money types, time, serde, error, config, auth stubs)
- `proto/` or `api/` (OpenAPI + generated types; or gRPC/prost)
- `ledger/` (service + core domain)
- `matcher/` (engine core + shard service)
- `gateway-ws/` (websocket gateway + subscriptions)
- `publisher-kafka/` (event publishing adapters)
- `admin/` (market creation/resolution/disputes/settlement)
- `testkit/` (fixtures, deterministic generators, in-proc harnesses)

### Hard Rules
- All money is integer minor units: CZK is integer already, so `i64` in CZK.
- Prices are fixed-point probability units: store as `i32` micros in `[0..1_000_000]`.
- Display conversion: `p/1e6 * 100 CZK`.
- Quantities are `i64` shares.
- Every command has `command_id` (UUIDv7 or ULID) for idempotency.
- Every fill has deterministic `fill_id` from `(shard, log_offset, match_index)` or UUID from matcher; ledger de-dupes.
- Determinism: matcher cannot read wall-clock for ordering; only uses command log order + provided timestamps for display.

## Core Events (Internal Domain)

### Commands
- `PlaceOrder`
- `CancelOrder`

### Matcher Outputs
- `OrderAccepted`
- `OrderRejected`
- `OrderRested`
- `OrderCanceled`
- `TradeExecuted`
- `OrderFilled` / `Partial`
- `BookLevelChanged`

### Ledger Outputs
- `ReservationCreated`
- `ReservationReleased`
- `ReservationAdjusted`
- `BalanceDebited`
- `BalanceCredited`
- `PositionUpdated`
- `FeeCharged`
- `SettlementApplied`

## Testing Baseline Across Services
- Unit tests: pure domain logic.
- Property tests (`proptest`): invariants (non-negative available, conservation, determinism).
- Integration tests: Postgres + Kafka via testcontainers; in-proc matcher+ledger harness.
- End-to-end tests: place orders -> trades -> portfolio views -> settlement, with crash/replay.

## Feature: Ledger with Reservations, Idempotent Transfers, Positions, Fees

### What It Does
Canonical custodian ledger for CZK balances, reserved balances, and outcome positions (long/short).

- Creates/adjusts/releases reservations at order acceptance and as fills occur.
- Applies each fill atomically: CZK transfer, positions, fees, and reservation release/adjustment.
- Enforces strict non-negative available balance: `available = balance - reserved >= 0` always.

### Data Model (Postgres)
- `accounts(user_id PK, balance_czk BIGINT, reserved_czk BIGINT, updated_at)`
- `positions(user_id, market_id, outcome_id, long_qty BIGINT, short_qty BIGINT, PK(user_id, market_id, outcome_id))`
- `reservations(reservation_id PK, user_id, market_id, outcome_id NULL, kind ENUM(buy,short), price_micros INT, qty_open BIGINT, amount_czk BIGINT, status, created_at)`
- `idempotency(command_id PK, applied_at, result_hash, type)`
- `applied_fills(fill_id PK, applied_at, shard_id, log_offset, payload_hash)`
- `fees(...)` optional audit table
- `transfers(ledger_entry_id PK, debit_user, credit_user, amount, reason, ref_id)`

For buy:
- `amount = qty_open * price_limit_czk_per_share + fee_reserve`

For short:
- `amount = qty_open * 100 * (1 - p_limit) + buffer`

### Core APIs (Internal)
- `ReserveForOrder(command_id, user_id, order_ref, kind, market_id, outcome_id, qty, price_limit_micros, fee_rate_bps, safety_buffer_bps) -> accepted/rejected + reservation_id`
- `ReleaseReservation(command_id, reservation_id, amount)` / `CloseReservation(command_id, reservation_id)`
- `ApplyFill(fill_id, taker, maker, market_id, outcome_id, qty, price_micros, taker_side, maker_order_ref, taker_order_ref, fee_rate_bps) -> applied/duplicate`

### Reservation Math
Buy reserve:
- `qty * (price_limit_micros/1e6)*100`, rounded up to CZK
- plus taker fee reserve: `qty * price_limit_czk * fee_bps/10_000`, rounded up

Short reserve:
- `qty * 100 * (1 - p_limit)`
- plus safety buffer: `ceil(base * buffer_bps/10_000)`
- no netting across outcomes initially

### Fill Application
If taker buys:
- Taker pays `qty * price_czk + fee`
- Maker receives `qty * price_czk`
- Positions: taker long `+= qty`; maker short decreases if previously short
- For maker sell behavior at launch: reduce long before increasing short for sells; reduce short before increasing long for buys while still reporting both

If taker sells:
- Symmetric
- Fee always charged to taker

Reservation adjustments:
- Buyer: reduce buy reservation by executed notional + executed fee; release remainder when order completes/cancels.
- Seller short: reduce short reservation as open qty decreases, proportionally using limit price; release difference.
- For sells that close existing long: acceptance can reserve full order qty at limit, then ledger releases excess after each fill using current position.

### Minimal Concise Doc (Deliverable)
- `docs/ledger.md`: invariants, schemas, rounding rules, idempotency rules, reservation lifecycle, fill application tables.

### Robust Tests
- Property: for any sequence of `Reserve/ApplyFill/Release`, `available >= 0`.
- Property: idempotent `ApplyFill` does not change state on duplicate `fill_id`.
- Unit: rounding edge cases (`p=0`, `p=1`, tiny qty, large qty).
- Integration: concurrent `Reserve` on same account uses SERIALIZABLE or advisory locks to prevent overspend.
- Crash test: apply fills, restart, reapply same fill stream, state unchanged.

## Feature: Single-Shard Matching Engine for One Outcome Book; Then Generalize

### What It Does
Deterministic price-time matching for each outcome order book.

- Accepts `PlaceOrder/CancelOrder` in total order (single writer per shard).
- Emits trade executions and book deltas.
- Persists command log and snapshots.

### Engine Core (Pure Rust Library)
- `Order`: `order_id, user_id, side, price_micros, qty_open, time_priority_seq, tif (GTC/IOC), self_trade_policy, reservation_id`
- Book:
- bids max-heap by price then seq
- asks min-heap by price then seq
- price levels map to FIFO queues

### Matching Algorithm
`PlaceOrder`:
- Validate price bands.
- Validate qty limits.
- If IOC: match opposing book until filled or limit reached; remainder cancels.
- If GTC limit: match as much as possible; remainder rests.

`CancelOrder`:
- Remove if open; emit canceled.

### Shard Service Responsibilities
- Maintain command log: append Place/Cancel with monotonic offset.
- On input, synchronously call ledger to create reservation before accepting order.
- If ledger rejects: emit `OrderRejected`.
- If ledger accepts: emit `OrderAccepted` and proceed to match/rest.
- After generating fills, call ledger `ApplyFill` per fill in deterministic order.
- On transient ledger failure, retry same `fill_id` until applied.

### Persistence
- Log: file segments (e.g., `.log`) with CRC, length-prefix.
- Snapshot: periodic serialized engine state (books + open orders + seq counters) to file/object storage; include `last_offset`.
- Recovery: load latest snapshot, replay log from `last_offset+1`.

### Generalize to N Outcomes and Multiple Markets
- Route by `market_id` to shard.
- Inside shard, map `market_id -> MarketState`.
- `MarketState` has N outcome books.
- Commands include `(market_id, outcome_id)`.

### Minimal Concise Doc
- `docs/matcher.md`: determinism rules, order priority, IOC walking, fill ordering, self-trade prevention, persistence formats, recovery.

### Robust Tests
- Golden tests: deterministic outputs for fixed command sequence (snapshot + event stream hash).
- Property: book never negative qty; no crossed book after processing.
- Property: price-time priority: earlier seq fills first at same price.
- Restart: commands -> snapshot -> restart -> replay gives bitwise identical state and outputs.
- Self-trade: opposing orders from same user do not match; explicit tested behavior.

## Feature: IOC Market-with-Slippage, Limit Orders, Cancellation

### What It Does
User API exposes:
- Limit: side, outcome, price, qty, `tif=GTC`
- Market-with-slippage: side, outcome, qty, `Pmax/Pmin` implemented as IOC + limit price

### Gateway HTTP API (No Frontend)
- `POST /orders`
- body: `{market_id, outcome_id, side, type: limit|market_slippage, price_micros?, qty, p_limit_micros?, client_order_id}`
- `DELETE /orders/{order_id}`
- `GET /orders`
- `GET /orders/{id}`

### Flow
- Gateway validates request, assigns `command_id`, routes to shard by `market_id`.
- Shard does ledger reservation, matcher, ledger fills, emits outputs.
- Gateway returns immediate ack with final IOC status (`filled/partial/canceled`) or accepted for resting.

### Minimal Concise Doc
- `docs/orders.md`: request fields, rounding, IOC semantics, partial fill behavior, cancel semantics, error codes.

### Robust Tests
- Integration: IOC buy walks multiple ask levels; confirm average execution and remainder canceled.
- Limit GTC: partial match then rest; then cancel; confirm reservation released.
- Fat-finger: price band violation -> reject, no reservation created.

## Feature: Sharding and Replication (Log + Snapshots), Then Downstream Kafka Distribution

### Sharding
Deterministic routing:
- `market_id % shard_count`
- override table for hot market migration

Migration approach:
- Stop accepting commands for market on old shard at `cutover_offset`.
- New shard loads snapshot + replays market log segment up to `cutover_offset`.
- Gateway routing changes to new shard starting at `cutover+1`.

### Replication
- Hot standby tails primary log (or rsync/object storage) and replays near real-time.
- Failover promotes replica at latest committed offset; gateway updates routing.

### Kafka Distribution (Downstream Only)
Topics:
- `marketdata.book.v1` (BookLevelChanged, trades)
- `orders.events.v1` (accepted/rested/canceled/filled)
- `ledger.events.v1` (balances/positions/reservations)
- `admin.resolution.v1` (market lifecycle)

Keying:
- marketdata keyed by `(market_id, outcome_id)`
- user streams keyed by `user_id`

Guarantees:
- Exactly-once not required; consumers de-dupe via `event_id`.
- Producer should be idempotent and include monotonic shard offsets.

### WebSockets Gateway
- Subscribes to Kafka marketdata + user orders + user ledger streams.
- Pushes with sequence numbers.
- Stateless clients reconnect and resync via REST snapshots (`GET /book`, `GET /portfolio`) plus optional Kafka offset hints.

### Minimal Concise Docs
- `docs/sharding.md`: routing, migration, replica replay, cutover invariants.
- `docs/kafka.md`: topics, schemas, partition keys, ordering guarantees.

### Robust Tests
- Shard replay determinism: log -> state hash match.
- Migration: move market, no lost/duplicated commands by `command_id`.
- Kafka consumer contracts: backward-compatible schema evolution, de-dupe behavior.

## Feature: Admin Tools for Market Creation, Resolution, Disputes, Settlement

### Market Creation
Store:
- `markets(market_id, N, outcomes, rules_json, sources_json, status, open/close times)`

Plus:
- price bands per outcome/event
- risk limits per market

### Resolution + Dispute
- `resolution_proposals(market_id, proposed_outcome_id, evidence_links, proposed_by, proposed_at)`
- `disputes(market_id, disputed_by, bond_czk, disputed_at, status, notes)`
- `final_resolution(market_id, outcome_id, finalized_at, finalized_by)`

### Settlement
For each user and outcome position:
- Winner outcome: long pays `+100*qty`; short pays `-100*qty`.
- Losing outcomes: long pays `0`; short pays `0` (short collateral released).

Implement as idempotent ledger batch:
- `ApplySettlement(market_id, settlement_id)` scans positions, posts transfers from house/escrow account, releases remaining reservations for that market, marks market settled.

Guidance:
- Use a house account for conservation + auditability.

### Minimal Concise Docs
- `docs/markets.md`: lifecycle, creation fields, close/freeze, cancellation rules.
- `docs/resolution.md`: proposal, dispute window, bonds, finalization, settlement math.

### Robust Tests
- Settlement invariants: after settlement, reserved for market is 0; balances reflect payouts; total CZK conserved vs house adjustments.
- Dispute flow: cannot finalize before window ends; bond lock/unlock rules enforced.

## Implementation Sequence with Concrete Milestones

### Milestone A: Ledger (2-3 weeks equivalent)
- Domain + Postgres schema + idempotency tables
- `ReserveForOrder` + `ApplyFill` + `Release`
- Unit + property + integration tests
- `docs/ledger.md`

### Milestone B: Matcher Single Market/Outcome
- Engine core + log + snapshot + recovery
- Ledger integration for reservation + fills
- REST gateway for orders/cancel + basic queries
- `docs/matcher.md`, `docs/orders.md`
- Determinism + restart tests

### Milestone C: N Outcomes + Multiple Markets
- `MarketState` map and routing
- Per-market limits + price bands
- update `docs/markets.md`
- broad integration tests

### Milestone D: Kafka + WebSockets + Portfolio Views
- Event producers from matcher/ledger outputs
- WebSocket subscriptions; REST snapshots
- `docs/kafka.md`

### Milestone E: Sharding + Replication + Migration
- Multi-shard deployment, routing table, migration replay
- Replica hot standby
- `docs/sharding.md`

### Milestone F: Admin + Resolution/Disputes + Settlement
- Admin API/CLI
- Settlement batch with idempotency
- `docs/resolution.md`

## Non-Negotiable Invariants
Assert everywhere (tests + runtime checks):
- `available_czk = balance - reserved >= 0` always.
- No order accepted without `reservation_id` (unless maker-free features later).
- Every fill applied at most once (`fill_id` unique) and refers to existing open orders.
- Matcher output stream is deterministic given command log.
- Book never crosses after processing (`best_bid < best_ask` unless empty).
- IOC remainder always canceled; GTC remainder always rested.
- Self-trade is prevented by explicit policy and tested.
