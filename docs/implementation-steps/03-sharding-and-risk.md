# Step 3 - Multi-Market, N-Outcome, Sharding, Risk Controls

## Objective

Scale matcher execution to multiple markets/outcomes with deterministic shard routing and enforce baseline risk limits.

## Scope

- Book manager keyed by `(market_id, outcome_id)`.
- Deterministic routing by `market_id % shard_count`.
- Operator-driven hot-market migration by replay.
- Per-user and per-market risk limits.

## Technical Design

### Routing and Shards

- Router maps command to shard using stable modulo hash.
- Each shard owns:
  - command queue,
  - single-thread event loop,
  - local market/outcome book map.
- No cross-shard shared mutable state.

### Risk Controls

- Per-user:
  - max open orders,
  - max qty per order,
  - max notional per order/event,
  - max short exposure (qty + CZK worst-case).
- Per-market/outcome:
  - static price bands `[min_tick, max_tick]`.
- Reject before reservation if command violates limits.

### Migration

- Freeze routing for market at command boundary.
- Copy source command log segment.
- Replay on target shard and compare state hash.
- Switch routing to target shard.

## Path Analysis (Routing, Risk, Migration)

### Happy Paths

- Router consistently maps commands for each market to the expected shard.
- Risk checks pass, reservation succeeds, and command execution proceeds deterministically.
- Hot-market migration replays to identical state hash and routing flips with no command loss.

### Bad/Failure Paths

- Commands violating user/market limits are rejected before reservation and before book mutation.
- Migration hash mismatch prevents cutover and keeps market on source shard.
- Commands with malformed or missing market/outcome metadata are rejected deterministically.
- Shard startup failure isolates only that shard and surfaces health signals without corrupting others.

### Edge Cases

- Boundary IDs around modulo transitions (`market_id` near shard bucket boundaries) remain stable across restarts.
- Burst load on one hot shard while other shards are idle does not violate per-shard single-writer guarantees.
- In-flight commands at freeze boundary are either fully included or excluded from migration segment (no split command effects).
- Risk limits at exact thresholds (equal to max) behave consistently with policy (`allow` vs `reject`).

## Implementation Tasks

1. Introduce book manager abstractions and shard runtime.
2. Extend command schema with market/outcome metadata.
3. Implement risk check pipeline before ledger reservation call.
4. Implement migration CLI/admin endpoint with state hash verification.
5. Add observability metrics per shard and risk reject counters.

## Test Plan

- Randomized multi-market simulations with invariant checks.
- Routing stability tests (same input => same shard mapping).
- Migration tests verifying source/target hash parity.
- Risk-limit tests for every rejection path.

## Acceptance Criteria

- Two-shard local environment runs deterministically.
- Migration completes with identical state hash and no lost commands.
- Risk policies reject invalid orders consistently.

## Deliverables

- shard runtime,
- multi-market/outcome support,
- migration tool,
- risk control module and tests.
