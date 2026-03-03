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
