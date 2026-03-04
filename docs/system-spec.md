# Custodial CZK N-way Prediction Exchange (CLOB) — Implementation Plan (Rust + Kafka + WebSockets)

Scope: backend only. Strong consistency, strict non-negative balances, conservative short collateral, deterministic matching engine per shard, append-only logs + snapshots, Kafka for downstream only, WebSockets for market/user streaming.

One share settles to 100 CZK if the outcome wins, else 0.

---

## 0) Top-level architecture

### Services
1) **Ledger Service (canonical)**
- Owns: CZK balances (available + reserved), positions (long/short per market/outcome), fees, settlement accounting.
- Guarantees: strict non-negative, idempotent application of fills/commands, atomic transfers.

2) **Matching Engine (single-writer per shard)**
- Owns: order books + open orders + order state transitions.
- Guarantees: deterministic price-time priority, IOC walking, log replay, snapshot recovery.
- Does NOT mutate money; it requests ledger reservations and submits fill intents for ledger to apply.

3) **Gateway API**
- HTTP for: place/cancel, user queries (balances/positions), market metadata.
- WebSockets for: user order/trade updates, market data streams.
- AuthN/Z, rate limiting, request idempotency keys.

4) **Kafka (downstream only)**
- Consumers: market data fanout, portfolio projections, notifications, audits, risk monitoring, admin dashboards.
- Correctness does not depend on Kafka.

5) **Admin/Resolution Service**
- Market creation, resolution posting, disputes/bonds, settlement triggering.
- Calls Ledger for settlement finalization.

### Storage
- Ledger DB: Postgres (recommended) with SERIALIZABLE txns, or FoundationDB if you want strict transactional semantics across keys.
- Matcher persistence: append-only **command log** + periodic **snapshot** files (local disk + object store).
- Kafka: event distribution only.

### Deterministic routing
- `shard = market_id % shard_count` for default.
- Support “hot market migration” by log replay into a dedicated shard.

---

## 1) Data model (core)

### Identifiers
- `UserId`, `MarketId`, `OutcomeId`, `OrderId`, `CommandId`, `FillId`, `TradeId`.
- All externally supplied state-changing requests must include a unique `CommandId` (idempotency key).

### Market / outcome
- Market has N outcomes. Each outcome has its own order book.
- Tick sizes:
    - Price tick: e.g. 0.0001 probability units (0.01 CZK per share).
    - Qty tick: integer shares.

### Orders
- `side`: Buy | Sell
- `type`: Limit | IOC_MarketableLimit
- `price`: limit price in [0,1] as fixed-point integer ticks.
- `qty_total`, `qty_remaining`
- `time_priority`: monotonic sequence number per book for determinism.

### Positions
Per user, per (market, outcome):
- `long_qty` (u64)
- `short_qty` (u64)
  No netting initially (treat long and short separately), or store as signed but compute margin as conservative worst-case.

### Wallet balances
- `czk_available` (i64 >= 0)
- `czk_reserved` (i64 >= 0)
  Invariant: `czk_available + czk_reserved == total_czk` and never negative.

### Reservations
Reservation records keyed by `(user_id, reservation_id)`:
- `kind`: BuyReserve | ShortCollateralReserve | FeeReserve
- `amount_czk`
- `linked_order_id` (optional)
- `status`: Active | Released | Consumed
  Support partial consumption and release.

---

## 2) Money math (must be exact)

Use integer math only.

- Face value: `FACE_CZK = 10000` if using “cents” (recommended); or 100 with halers if you want.
- Represent price as integer ticks: `p_ticks` where `p = p_ticks / P_SCALE`.
- Settlement payout per share: `FACE_CZK` for winner else 0.

### Buy reservation
For order qty `Q` and limit `P_limit`:
- `reserve = Q * price_to_czk(P_limit) + fee_max`
- If taker-only fee rate is `fee_bps`, compute worst-case fee against reserved notional:
    - `notional_max = Q * price_to_czk(P_limit)`
    - `fee_max = ceil(notional_max * fee_bps / 10000)`
      Reserve at acceptance. Release unused after fills/cancel.

### Short collateral reservation (conservative)
For a sell that creates/extends short, reserve worst-case loss:
- Proceeds: `Q * price_to_czk(P_limit_or_exec)`
- Obligation if wins: `Q * FACE_CZK`
- Worst-case loss: `Q * (FACE_CZK - price_to_czk(P))`
  At order acceptance for resting sell limits, reserve on **limit price** for full unfilled qty:
- `collat = Q_unfilled * (FACE_CZK - price_to_czk(P_limit))`
  Add buffer: `collat *= (1 + buffer_bps/10000)` rounded up.

As fills happen, reduce required collateral proportionally and release the excess.

Important: fees on sells are charged on proceeds/notional; reserve separately or bake into required collateral.

---

## 3) API surface (backend-only)

### HTTP (Gateway)
- `POST /orders` PlaceOrder
- `DELETE /orders/{order_id}` CancelOrder
- `GET /markets`, `GET /markets/{id}`, `GET /books/{market}/{outcome}`
- `GET /me/balances`, `GET /me/positions`, `GET /me/orders`

Requests must include:
- `command_id` (UUID)
- `client_order_id` (optional, for UX)
- auth token (JWT / session)

### WebSockets
Two channels:
1) Market data:
- `book_top`, `book_depth` (optional), `trades`, `mark_price` (derived)
2) User:
- `order_updates`, `fills`, `balance_updates`, `position_updates`

WebSocket server consumes Kafka projections OR directly subscribes to internal fanout from matcher/ledger. If Kafka is delayed, user channel should prefer direct internal events for responsiveness (but do not make correctness depend on it).

---

## 4) Matching Engine (deterministic state machine)

### Inputs (totally ordered per shard)
- `PlaceOrder(cmd)`
- `CancelOrder(cmd)`

### Outputs (append-only)
- `OrderAccepted | OrderRejected(reason)`
- `OrderRested`
- `OrderCanceled`
- `TradeExecuted{maker_order_id,taker_order_id, price, qty, fill_id}`
- `OrderPartial | OrderFilled`
- `BookLevelChanged`

### Core invariants
- Determinism: same input log => same outputs.
- Price-time priority: best price first; within same price FIFO.
- IOC marketable-limit: walk book until filled or limit reached; remainder canceled.
- Self-trade prevention: do not match orders with same user_id. Policy: cancel taker remainder or skip resting order (choose and document one; recommend “skip and continue” for depth-walking, but ensure determinism).

### Matching algorithm (per outcome book)
For taker order:
- Determine crossing condition:
    - Buy crosses asks with `ask_price <= limit_price`
    - Sell crosses bids with `bid_price >= limit_price`
- Iterate price levels in best-to-worse order while crossing and qty_remaining > 0:
    - Match against FIFO queue at that level.
    - Emit `TradeExecuted` per match chunk with unique `fill_id`.
    - Update maker remaining, remove if filled.
    - Update taker remaining.
- If limit order and remaining > 0:
    - Rest on book (after reservation already secured).
- If IOC and remaining > 0:
    - Cancel remainder.

### Reservation / ledger interaction model (strong consistency)
At order acceptance, matcher must ensure reservations exist BEFORE resting/IOC matching proceeds. Two viable patterns:

Pattern A (recommended): **Synchronous reservation check**
1) Gateway sends PlaceOrder to matcher.
2) Matcher calls ledger `ReserveForOrder` (sync RPC) with computed worst-case reserve.
3) If ledger OK: matcher appends `OrderAccepted` and continues.
4) If ledger rejects: matcher emits `OrderRejected(INSUFFICIENT_FUNDS)`.

Pattern B: Two-phase via ledger-first
1) Gateway calls ledger to create reservation.
2) Gateway sends order with `reservation_id` to matcher.
3) Matcher trusts reservation_id exists and is active.

Pattern A is simpler operationally and keeps matcher the sole sequencer for user commands per market shard. Use gRPC between matcher and ledger.

---

## 5) Ledger service (canonical balances/positions)

### Responsibilities
- Maintain balances and reservations; enforce non-negative.
- Apply fills atomically:
    - Transfer CZK between buyer and seller (proceeds).
    - Charge taker fee.
    - Update long/short positions.
    - Consume/release reservations as appropriate.
- Idempotency:
    - `command_id` for reservation operations
    - `fill_id` for fill application
    - store processed ids with unique constraints.

### Ledger RPCs (gRPC)
- `ReserveForOrder(command_id, user_id, order_id, kind, amount_czk) -> ok|reject`
- `AdjustReservation(order_id, delta_czk)` (downward releases typically)
- `ReleaseReservation(order_id)` (on cancel/unfilled remainder)
- `ApplyFill(fill_id, maker, taker, price_czk, qty, taker_side, order_ids, fees) -> ok`
- `GetBalances/GetPositions`

### Fill application details
For each executed trade at price `P_exec` and qty `Q`:
- Notional CZK = `Q * price_to_czk(P_exec)`
- Buyer pays notional; seller receives notional.
- Fee: charged to taker only:
    - `fee = ceil(notional * fee_bps / 10000)`
- Reservation consumption:
    - Buyer: consume from buy reserve (notional + fee).
    - Seller short collateral:
        - If seller is shorting, consume/lock collateral reserve (already reserved on limit). On fill, update short position and recompute required collateral; release excess if exec price improves vs limit.
    - Maker sell that is reducing long: proceeds are just credited; no collateral needed.

Position updates (no netting):
- If user buys: increase long_qty by Q (or reduce short if you implement netting later).
- If user sells:
    - If has long_qty >= Q: decrease long_qty by Q
    - Else:
        - sell_long = long_qty
        - short_add = Q - sell_long
        - long_qty becomes 0; short_qty += short_add
        - collateral must cover full short_qty at relevant basis (use conservative “per-order reserved” model at launch).

Note: If you truly avoid netting, selling while long would still create short in your model; that is undesirable. Recommend allowing “reduce long first” as above while still using conservative collateral for any remaining short.

---

## 6) Risk controls (launch-minimum)

Enforce in Gateway or Matcher (prefer matcher for determinism), and ledger for monetary constraints.

Per-user:
- Max open orders
- Max qty per order
- Max notional per event
- Max short exposure per event (in shares and in worst-case CZK)

Per event/outcome:
- Price band:
    - For each outcome, maintain `min_price_tick` and `max_price_tick` at creation time.
    - Reject orders outside bands.
- Marketable limit sanity:
    - Reject IOC buys with Pmax above max band; similarly sells with Pmin below min band.

Self-trade prevention:
- Reject match if maker.user_id == taker.user_id.
- Policy: skip that maker order and continue walking. If all remaining liquidity is self, IOC remainder cancels.

---

## 7) Persistence, replay, replication

### Matcher persistence
- Append-only command log per shard (or per market for easy migration):
    - Record inputs: PlaceOrder, CancelOrder with full payload.
    - Record outputs optionally for audit; outputs can be recomputed but storing helps debugging.
- Snapshot:
    - Periodically serialize books + open orders + sequence counters to snapshot file.
    - Recovery: load latest snapshot + replay commands after snapshot offset.

### Hot standby
- Replica process replays the same log and maintains identical state.
- Leader election outside scope (use etcd/consul). Ensure only one writer.

### Migration
- Copy log segment for market to new shard; replay to build state; cutover by routing switch at a command boundary.

---

## 8) Kafka topics (downstream only)

Suggested topics (partition by market_id for order):
- `md.book_level_changed` (compacted optional)
- `md.trades`
- `user.order_updates` (partition by user_id)
- `user.fills` (partition by user_id)
- `user.balance_updates`
- `audit.matcher_events`
- `audit.ledger_events`
- `admin.market_events`
- `admin.resolution_events`

Schema: protobuf or Avro with explicit versioning. Include monotonic `event_seq` per source for consumer ordering.

---

## 9) Resolution, disputes, settlement (A+B+C hybrid)

### Market metadata
At creation:
- Primary + secondary sources (URLs stored as text)
- Explicit criteria and tie-break rules (stored immutable)
- Dispute window duration
- Dispute bond amount (CZK)

### Resolution workflow
1) Operator posts `proposed_outcome_id`, timestamp, evidence links.
2) Dispute window opens.
3) If dispute submitted with bond:
    - mark `in_review`, freeze finalization.
    - follow rulebook; operator decides bounded by sources/criteria.
4) Finalize outcome.

### Settlement (ledger-driven)
For each market:
- For each user outcome position:
    - Winner long: credit `long_qty * FACE_CZK`
    - Loser long: credit 0
    - Winner short: debit `short_qty * FACE_CZK`
    - Loser short: debit 0
- Release all remaining reservations tied to that market.
- Ensure strict non-negative: shorts should already have collateral reserved; settlement consumes reserved then available if needed (but should not be needed if model is correct). If still insufficient due to bug or admin adjustment, mark account as deficit and block withdrawals/trading (operator policy).

---

## 10) Implementation sequence (with acceptance criteria + tests)

### Phase 1 — Ledger MVP (reservations, idempotency, fills)
Deliverables:
- Postgres schema + migrations.
- gRPC ledger API.
- Reservation engine:
    - Create, adjust, release.
    - Enforce available >= reserve.
- Fill application:
    - atomic balance transfer
    - taker fee
    - position updates (reduce-long-first, else short)
    - idempotency by fill_id unique constraint.

Testing (robust):
- Unit tests:
    - price_to_czk conversions and rounding rules
    - fee rounding
    - collateral formulas
- DB transaction tests:
    - concurrent reservations against same wallet (SERIALIZABLE) never allow negative
    - idempotent ApplyFill (same fill_id twice is no-op)
- Property tests (proptest):
    - invariant: available/reserved never negative
    - invariant: total CZK conserved minus fees
    - random sequences of reserve/adjust/release/applyfill

Acceptance criteria:
- All invariants hold under concurrency test with 100+ parallel tasks.
- Deterministic results for same sequence.

### Phase 2 — Single outcome matcher (one book) + synchronous reservations
Deliverables:
- Matcher process with:
    - in-memory book
    - PlaceOrder/CancelOrder
    - deterministic matching
    - sync ReserveForOrder calls
- Command log + snapshot.
- Basic HTTP gateway to submit commands and query state (no websockets yet).

Testing:
- Golden tests:
    - given input sequence, compare emitted trades/book states to fixtures
- Determinism tests:
    - replay log twice => identical hash of final state
- Matching correctness:
    - price-time priority
    - partial fills across multiple levels
    - IOC remainder cancels
    - self-trade skip behavior deterministic

Acceptance criteria:
- All golden tests pass; replay stable.

### Phase 3 — N outcomes + multiple markets + sharding
Deliverables:
- Book manager keyed by (market_id,outcome_id).
- Shard router (market_id % shard_count).
- Market migration by log replay (operator-driven).
- Limits: max open orders, size, notional, short exposure, price bands.

Testing:
- Simulation tests:
    - random markets/outcomes/orders; check invariants
- Shard tests:
    - routing stable
    - migration preserves state hash

Acceptance criteria:
- Can run 2 shards locally; migrate one market without mismatches.

### Phase 4 — Kafka downstream + WebSockets
Deliverables:
- Kafka producer in matcher and ledger (or gateway) emitting events.
- WebSocket server:
    - market channels (trades, top-of-book, optionally depth)
    - user channels (order/fill/balance updates)
- Backpressure handling:
    - bounded queues
    - drop policy for non-critical market depth snapshots

Testing:
- Contract tests for Kafka schemas (backward compatible).
- WebSocket integration tests:
    - connect, subscribe, receive expected stream on trades/cancels
- Soak test:
    - sustained order flow; ensure memory bounded.

Acceptance criteria:
- Kafka consumers can reconstruct top-of-book and last trades.
- WebSocket stays stable under load.

### Phase 5 — Admin + Resolution + Settlement
Deliverables:
- Admin API:
    - create market (N outcomes, sources, criteria, bands, fees)
    - post resolution, dispute, finalize
    - trigger settlement
- Ledger settlement routine:
    - iterate positions, apply payouts, release reserves

Testing:
- Settlement tests:
    - long winner paid, loser zero
    - short winner debited, loser zero
    - reserves released
    - idempotent settlement (run twice no change)
- Dispute window timing tests.

Acceptance criteria:
- End-to-end market lifecycle in integration test.

---

## 11) Rust stack choices (recommended)

- gRPC: `tonic`
- HTTP: `axum`
- WebSockets: `axum::extract::ws` or `tokio-tungstenite`
- Kafka: `rdkafka`
- Postgres: `sqlx` (compile-time checked queries) or `tokio-postgres`
- Serialization: `prost` for protobuf (also for Kafka schemas)
- Persistence: snapshots via `bincode` or `rkyv` (choose one; document versioning)
- Testing:
    - `proptest` for property tests
    - `tokio::test` for async
    - `testcontainers` for Postgres/Kafka integration tests

---

## 12) Operational notes (minimal but necessary)

- All monetary fields in smallest currency unit (integer).
- All matcher operations single-threaded per shard for determinism; use message queue + one executor thread.
- Ledger uses SERIALIZABLE or explicit row locking on wallet rows (`SELECT ... FOR UPDATE`) to prevent race conditions.
- Observability:
    - structured logs with `command_id`, `order_id`, `fill_id`
    - metrics: order rate, reject rate, ledger tx latency, replay lag, websocket subscribers

---

## 13) Definition of done (launch core)

- Place/cancel orders (limit + IOC marketable limit).
- Strict non-negative balances with reservations.
- Conservative short collateral with buffer.
- Deterministic matcher with replay + snapshot recovery.
- Taker-only fees enforced and reserved.
- Basic risk limits + self-trade prevention.
- Kafka + WebSockets for downstream/UX.
- Admin lifecycle: create market, resolve, dispute, settle.
- Comprehensive unit + property + integration tests; CI runs all.

---