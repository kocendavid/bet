ALTER TABLE reservations
  ADD COLUMN IF NOT EXISTS market_id TEXT NOT NULL DEFAULT '',
  ADD COLUMN IF NOT EXISTS outcome_id TEXT NOT NULL DEFAULT '';

CREATE INDEX IF NOT EXISTS reservations_market_status_idx
  ON reservations(market_id, status, reservation_id);

CREATE TABLE IF NOT EXISTS settlement_runs (
  market_id TEXT PRIMARY KEY,
  idempotency_key TEXT NOT NULL,
  winning_outcome_id TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('in_progress', 'completed')),
  last_user_id TEXT NOT NULL DEFAULT '',
  last_reservation_id TEXT NOT NULL DEFAULT '',
  processed_users BIGINT NOT NULL DEFAULT 0 CHECK (processed_users >= 0),
  released_reservations BIGINT NOT NULL DEFAULT 0 CHECK (released_reservations >= 0),
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
