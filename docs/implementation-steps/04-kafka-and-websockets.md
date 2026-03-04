# Step 4 - Kafka Downstream and WebSocket Streaming

## Objective

Provide low-latency streaming outputs for users and market data while preserving correctness independence from Kafka.

## Scope

- Kafka producer integration for matcher and ledger events.
- WebSocket server for market/user channels.
- Backpressure handling with bounded queues and drop policies.

## Technical Design

### Kafka Topics

- `md.book_level_changed` (market partition key)
- `md.trades` (market partition key)
- `user.order_updates` (user partition key)
- `user.fills` (user partition key)
- `user.balance_updates` (user partition key)
- `audit.matcher_events`, `audit.ledger_events`

### Event Contract

- Protobuf schema with explicit version field and `event_seq`.
- Producer guarantees in-order sends per partition key.
- Consumer compatibility rule: backward-compatible schema changes only.

### WebSocket Channels

- Market stream: trades, top-of-book, optional depth snapshots.
- User stream: order updates, fills, balance and position updates.
- Prefer direct internal fanout for user responsiveness; Kafka-backed projection allowed for market aggregates.

### Backpressure

- Per-connection bounded outbound queue.
- Drop/coalesce policy only for non-critical depth snapshots.
- Never drop user fills/order state deltas.

## Path Analysis (Streaming and Delivery)

### Happy Paths

- Producer publishes ordered events per partition key and consumers reconstruct correct market/user timelines.
- Authorized WebSocket client subscribes and receives ordered, lossless critical user updates.
- Market stream coalesces non-critical depth snapshots while preserving trade and top-of-book correctness.

### Bad/Failure Paths

- Kafka unavailable or transiently failing triggers retries/metrics while core matching correctness continues.
- Unauthorized subscription attempts are rejected without leaking channel data.
- Slow consumer exceeds queue bounds and receives controlled degradation/disconnect per policy.
- Schema-incompatible event is rejected by contract validation and surfaced through dead-letter metrics.

### Edge Cases

- Rapid connect/disconnect churn does not leak subscriptions or queue buffers.
- Out-of-order arrival from mixed fanout sources is normalized by `event_seq` where required.
- Reconnect with last-seen sequence handles small replay windows without duplicate state effects.
- One noisy market stream does not starve user-critical streams under shared resource pressure.

## Implementation Tasks

1. Define protobuf event schemas and versioning policy docs.
2. Implement Kafka producer wrappers with retries and dead-letter metrics.
3. Implement WebSocket subscription manager and channel authorization.
4. Implement fanout adapters (internal bus + optional Kafka projection consumer).
5. Add load-shedding and queue instrumentation.

## Test Plan

- Kafka contract compatibility tests.
- Integration tests for stream subscription and message ordering.
- Load tests for sustained order flow and connection churn.
- Memory-bound tests for queue backpressure behavior.

## Acceptance Criteria

- Consumers can reconstruct top-of-book and trade tape.
- User channels deliver ordered, lossless critical updates.
- Service remains stable under soak/load tests.

## Deliverables

- kafka event schemas + producers,
- websocket service,
- integration/load test artifacts,
- operational metrics dashboards.
