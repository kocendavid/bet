# Technical Implementation Plan

## Goal
Build the prediction market platform in execution-safe increments while preserving determinism, idempotency, and accounting invariants.

## Execution Model
- Branching: create one branch per milestone (`codex/milestone-a-ledger`, etc.).
- CI gate per milestone: `fmt + clippy + unit + integration`.
- Definition of done for every milestone:
- Docs updated
- Invariants encoded as tests
- Crash/replay test included where applicable

## Milestone 0: Foundation and Docker Compose (Week 0)

### Scope
Set up workspace, local runtime dependencies, and baseline observability/dev tooling.

### Tasks
1. Initialize Rust workspace
- Create crates: `common`, `ledger`, `matcher`, `gateway-ws`, `publisher-kafka`, `admin`, `testkit`.
- Add strict lint config (`clippy::pedantic` where practical).
- Add `rust-toolchain.toml` with pinned stable toolchain.

2. Add containerized dependencies (`docker-compose.yml`)
- Services:
- `postgres` (ledger/admin canonical state)
- `kafka` + `redpanda-console` (or kafka + zookeeper if preferred)
- `minio` (+ optional `mc` init job)
- `prometheus` + `grafana` (optional but recommended in dev)
- Expose ports and healthchecks.
- Add named volumes for persistence.

3. Add migration and local scripts
- `scripts/dev-up.sh`, `scripts/dev-down.sh`, `scripts/dev-reset.sh`.
- `scripts/wait-for-deps.sh`.
- `Makefile` targets: `make up`, `make down`, `make test`, `make itest`.

4. Baseline test harness
- Testcontainers wiring for Postgres/Kafka in Rust integration tests.
- Shared fixtures in `testkit`.

### Deliverables
- `docker-compose.yml`
- `.env.example`
- workspace `Cargo.toml`
- migration scaffolding
- `docs/dev-setup.md`

### Acceptance Criteria
- `docker compose up -d` starts all dependencies healthy.
- One smoke integration test can read/write Postgres and publish/consume Kafka.

## Milestone A: Ledger (Weeks 1-3)

### Scope
Implement canonical accounting, reservations, fill application, and idempotency guarantees.

### Tasks
1. Schema and migrations
- Tables: `accounts`, `positions`, `reservations`, `idempotency`, `applied_fills`, `transfers`, optional `fees`.
- Constraints:
- `balance_czk >= 0`, `reserved_czk >= 0`, `balance_czk >= reserved_czk`
- unique keys for idempotency (`command_id`) and fill dedupe (`fill_id`).

2. Domain services
- `ReserveForOrder`
- `ReleaseReservation` / `CloseReservation`
- `ApplyFill`
- Deterministic rounding helpers in `common`.

3. Concurrency controls
- Use SERIALIZABLE transactions for reservation and fill paths.
- Add fallback advisory lock on `user_id` for overspend-sensitive sections.

4. Idempotency
- Command idempotency table with `result_hash`.
- Fill idempotency with payload hash mismatch detection.

5. Tests
- Property tests for `available >= 0`.
- Duplicate fill tests.
- Rounding edge-case tests.
- Concurrent reserve integration test.
- Crash/replay idempotency test.

### Deliverables
- `docs/ledger.md`
- SQL migrations
- ledger crate API + tests

### Exit Criteria
- Non-negative available invariant enforced in DB + service.
- Duplicate `ApplyFill` is no-op.

## Milestone B: Matcher Single Market/Outcome (Weeks 4-5)

### Scope
Build deterministic single-shard matching engine with log/snapshot recovery and ledger coupling.

### Tasks
1. Engine core
- In-memory order book with strict price-time priority.
- IOC and GTC execution.
- Self-trade prevention policy (explicit mode + tests).

2. Command log and snapshots
- Append-only segment format with CRC and length prefix.
- Snapshot serialization with `last_offset` and counters.

3. Shard service orchestration
- Ordered command intake.
- Reserve-before-accept flow with ledger.
- Deterministic fill emission and retry-until-applied ledger calls.

4. Recovery
- Load latest snapshot, replay log tail.
- Deterministic state hash for validation.

5. API skeleton
- Minimal HTTP endpoints in gateway for place/cancel/query.

### Deliverables
- `docs/matcher.md`
- `docs/orders.md`
- golden tests for deterministic output

### Exit Criteria
- Replay yields identical state/event hash.
- Book invariants always hold.

## Milestone C: N Outcomes + Multi-Market (Weeks 6-7)

### Scope
Generalize matcher and routing to multiple outcomes and markets.

### Tasks
1. Market state generalization
- `HashMap<MarketId, MarketState>` with per-outcome books.
- Routing by `(market_id, outcome_id)`.

2. Risk controls
- Price bands, size limits, market status checks.

3. Gateway routing
- Deterministic market-to-shard routing abstraction.

4. Test expansion
- Multi-market integration and cross-market isolation tests.

### Deliverables
- `docs/markets.md` updates
- multi-market test suite

### Exit Criteria
- Commands on one market cannot mutate another market state.

## Milestone D: Kafka + WebSockets + Portfolio Views (Weeks 8-9)

### Scope
Publish downstream event streams and expose real-time + snapshot client access.

### Tasks
1. Event contracts
- Define versioned schemas for `marketdata.book.v1`, `orders.events.v1`, `ledger.events.v1`, `admin.resolution.v1`.

2. Publishers
- Kafka producers with event IDs and shard offset metadata.
- Key by `(market_id, outcome_id)` or `user_id` as specified.

3. WS gateway
- Subscription auth and fanout.
- Sequence numbers and reconnect semantics.

4. Snapshot REST endpoints
- `/book`
- `/portfolio`
- include offset/version hints.

5. Contract tests
- Backward-compatible schema checks and consumer dedupe behavior.

### Deliverables
- `docs/kafka.md`
- WS gateway crate implementation

### Exit Criteria
- Client can recover from disconnect via snapshot + stream continuation.

## Milestone E: Sharding, Replication, Migration (Weeks 10-12)

### Scope
Operate multiple shards with deterministic routing and safe market migration.

### Tasks
1. Routing service
- `market_id % shard_count` + override table.

2. Replica mode
- Log tail + replay into standby shard.
- Promote standby by committed offset.

3. Market migration workflow
- Freeze old shard at `cutover_offset`.
- Replay on target shard through cutover.
- Flip routing at `cutover+1`.
- Validate command_id continuity.

4. Operational tooling
- Admin CLI commands for freeze/migrate/promote.

5. Tests
- Lost/duplicate command prevention in migration simulation.
- Replay determinism hash checks.

### Deliverables
- `docs/sharding.md`
- migration runbook

### Exit Criteria
- Migration completes with zero missing/duplicate command IDs.

## Milestone F: Admin, Resolution, Disputes, Settlement (Weeks 13-14)

### Scope
Add market lifecycle management and idempotent settlement.

### Tasks
1. Admin APIs
- Market creation/update/freeze/close.
- Resolution proposal and dispute lifecycle.

2. Settlement engine
- `ApplySettlement(market_id, settlement_id)` idempotent batch.
- Release all market-tied reservations.
- Transfer payouts via house/escrow account.

3. Dispute and timing rules
- Enforce dispute window before finalization.
- Bond lock/unlock transitions.

4. Tests
- Conservation checks with house account.
- Post-settlement zero reservation for settled market.
- Dispute rule enforcement tests.

### Deliverables
- `docs/resolution.md`
- settlement batch implementation

### Exit Criteria
- Settlement re-run is no-op and balances remain stable.

## Cross-Cutting Workstreams
- Observability: structured logs, metrics, trace IDs by `command_id` and `fill_id`.
- Security: auth stubs -> JWT validation and role checks for admin APIs.
- Performance: benchmark matcher hot path and DB transaction latency.
- Backups: snapshot/log retention + MinIO lifecycle policies.

## Subagent Execution Plan
If using subagents, assign one per milestone in parallel with a principal integrator:
- Subagent A: Ledger and accounting invariants
- Subagent B: Matcher core and determinism
- Subagent C: Gateway/API and validation
- Subagent D: Kafka/WS contracts and delivery semantics
- Subagent E: Sharding/replication/migration tooling
- Subagent F: Admin/resolution/settlement workflows
- Integrator: merges, interface contracts, CI stability, release notes

## Immediate Next Actions
1. Implement Milestone 0 artifacts (`docker-compose.yml`, scripts, workspace skeleton).
2. Lock API contract format choice (OpenAPI vs gRPC/prost) before Milestone B.
3. Start Milestone A with migrations + `ReserveForOrder` path first.
