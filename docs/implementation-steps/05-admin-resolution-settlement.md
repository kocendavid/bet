# Step 5 - Admin APIs, Resolution Workflow, Settlement

## Objective

Implement full market lifecycle controls: creation, dispute-aware resolution, and deterministic settlement.

## Scope

- Admin APIs for market creation and lifecycle actions.
- Resolution/dispute workflow with time-window enforcement.
- Ledger settlement routine that applies payouts/debits and releases reserves.

## Technical Design

### Admin API

- `POST /admin/markets` create market with:
  - outcomes,
  - sources,
  - criteria,
  - dispute window,
  - price bands,
  - fee config.
- `POST /admin/markets/{id}/resolution/propose`
- `POST /admin/markets/{id}/disputes`
- `POST /admin/markets/{id}/resolution/finalize`
- `POST /admin/markets/{id}/settle`

### Resolution State Machine

- `Open -> Proposed -> DisputeWindow -> Finalized -> Settled`
- If dispute filed with valid bond during dispute window:
  - transition to `InReview`, block finalization until decision.

### Settlement Logic

- For each user position in market:
  - winner long: credit `long_qty * FACE_CZK`
  - loser long: credit `0`
  - winner short: debit `short_qty * FACE_CZK`
  - loser short: debit `0`
- Release all residual market-linked reservations.
- Idempotency key per market settlement run.

## Path Analysis (Lifecycle and Settlement)

### Happy Paths

- Admin creates market with valid config and state transitions `Open -> Proposed -> DisputeWindow -> Finalized -> Settled`.
- No valid dispute during window allows timely finalization and deterministic settlement.
- Settlement worker processes all position rows, applies payouts/debits, releases reservations, and records completion checkpoint.

### Bad/Failure Paths

- Unauthorized lifecycle action is rejected without state transition.
- Finalize attempts before dispute window close are rejected.
- Valid dispute filed in window transitions market to `InReview` and blocks finalization.
- Duplicate settle command/rerun is idempotent and produces no net monetary drift.

### Edge Cases

- Boundary timestamp actions at dispute window open/close are evaluated consistently in one timezone policy.
- Large markets settle in chunks across worker restarts without skipping or double-processing users.
- Mixed user portfolios (long+short across outcomes) conserve total payout/debit invariants.
- Late duplicate admin requests (retries) are safely de-duplicated by idempotency keys.

## Implementation Tasks

1. Add admin auth and role checks for lifecycle endpoints.
2. Implement market metadata persistence and immutable criteria storage.
3. Implement resolution/dispute state transitions with timestamp validation.
4. Implement settlement worker with chunked processing and progress checkpointing.
5. Emit audit events for every lifecycle transition.

## Test Plan

- Resolution timing tests for dispute window boundaries.
- Settlement correctness tests for winning/losing long and short positions.
- Idempotent settlement tests (repeat run no net change).
- End-to-end market lifecycle integration test.

## Acceptance Criteria

- Full lifecycle executes from market create to settle in integration tests.
- Settlement is idempotent and releases all related reservations.
- Dispute workflow blocks/permits finalization exactly per policy.

## Deliverables

- admin lifecycle APIs,
- resolution/dispute engine,
- settlement worker,
- full-lifecycle integration test suite.
