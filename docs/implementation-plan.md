# Technical Implementation Plan

This plan decomposes implementation into five sequential steps. Each step has its own technical plan file with explicit scope, tasks, interfaces, tests, and acceptance criteria.

## Step Files

1. [Step 1 - Ledger MVP](implementation-steps/01-ledger-mvp.md)
2. [Step 2 - Deterministic Matcher MVP](implementation-steps/02-matcher-mvp.md)
3. [Step 3 - Multi-Market, N-Outcome, Sharding](implementation-steps/03-sharding-and-risk.md)
4. [Step 4 - Kafka and WebSockets](implementation-steps/04-kafka-and-websockets.md)
5. [Step 5 - Admin, Resolution, Settlement](implementation-steps/05-admin-resolution-settlement.md)

## Execution Order and Gates

- Execute strictly in order from Step 1 to Step 5.
- Do not start a later step until all acceptance criteria in the current step pass.
- Each step must produce:
  - updated migrations and API contracts,
  - deterministic and repeatable tests,
  - operational notes for deployment/runbooks.

## Branching and Delivery

- Create one branch per step: `codex/step-<n>-<slug>`.
- Open one PR per step; include:
  - implementation summary,
  - test evidence,
  - migration/rollout notes,
  - known limitations.

## Cross-Step Technical Rules

- Money math is integer-only (smallest CZK unit).
- All write operations must be idempotent (`command_id`, `fill_id`, settlement id).
- Ledger remains the monetary source of truth.
- Matcher remains single-writer per shard for determinism.
- Kafka is downstream only (no correctness dependency).
