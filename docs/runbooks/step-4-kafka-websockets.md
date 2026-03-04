# Step 4 Runbook: Kafka and WebSocket Streaming

## Scope Source

This step is implemented strictly from `docs/implementation-steps/04-kafka-and-websockets.md`.

## Event Contract

- Protobuf contract: `proto/streaming.proto` (`stream.v1.EventEnvelope`).
- Contract invariants:
  - `version` is explicit and currently `1`.
  - `event_seq` is monotonic per partition key (`market_id` for market streams, `user:{user_id}` for user streams).
  - New schema changes must be backward-compatible (additive fields only, no tag reuse).

## Delivery Model

- Internal fanout (`StreamHub`) is the primary low-latency path for user-critical events.
- Kafka is downstream-only and wrapped behind retry/dead-letter behavior (`matcher-service/src/kafka.rs`).
- If Kafka is unhealthy, matcher correctness and user-critical delivery continue via internal fanout.

## Backpressure and Load Shedding

- Every WebSocket connection has a bounded queue.
- Critical events (user order deltas, fills, market trades/top-of-book) are never dropped.
  - If queue is full for critical events, the connection is disconnected.
- Non-critical depth snapshots are dropped under pressure.
- Metrics are exposed in `/admin/metrics` under `streaming`:
  - `active_connections`
  - `unauthorized`
  - `delivered`
  - `depth_dropped`
  - `slow_consumer_disconnects`

## Validation Summary

- Contract compatibility tests verify additive evolution compatibility for protobuf envelopes.
- Stream tests verify ordered/lossless critical user updates.
- Backpressure tests verify depth drops and slow-consumer disconnect behavior.
- Load/churn tests verify subscription stability under sustained publish/unsubscribe activity.
