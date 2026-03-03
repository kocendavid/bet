CREATE TABLE IF NOT EXISTS wallets (
  user_id TEXT PRIMARY KEY,
  available_czk BIGINT NOT NULL DEFAULT 0 CHECK (available_czk >= 0),
  reserved_czk BIGINT NOT NULL DEFAULT 0 CHECK (reserved_czk >= 0),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS reservations (
  reservation_id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES wallets(user_id) ON DELETE RESTRICT,
  order_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  amount_czk BIGINT NOT NULL CHECK (amount_czk >= 0),
  consumed_czk BIGINT NOT NULL DEFAULT 0 CHECK (consumed_czk >= 0),
  status TEXT NOT NULL CHECK (status IN ('active', 'released', 'consumed')),
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  UNIQUE (order_id, kind)
);

CREATE INDEX IF NOT EXISTS reservations_user_idx ON reservations(user_id);

CREATE TABLE IF NOT EXISTS positions (
  user_id TEXT NOT NULL,
  market_id TEXT NOT NULL,
  outcome_id TEXT NOT NULL,
  long_qty BIGINT NOT NULL DEFAULT 0 CHECK (long_qty >= 0),
  short_qty BIGINT NOT NULL DEFAULT 0 CHECK (short_qty >= 0),
  PRIMARY KEY(user_id, market_id, outcome_id)
);

CREATE TABLE IF NOT EXISTS fills (
  fill_id TEXT PRIMARY KEY,
  maker_user_id TEXT NOT NULL,
  taker_user_id TEXT NOT NULL,
  maker_order_id TEXT NOT NULL,
  taker_order_id TEXT NOT NULL,
  qty BIGINT NOT NULL CHECK (qty > 0),
  price_czk BIGINT NOT NULL CHECK (price_czk >= 0),
  notional_czk BIGINT NOT NULL CHECK (notional_czk >= 0),
  fee_czk BIGINT NOT NULL CHECK (fee_czk >= 0),
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS processed_commands (
  command_id TEXT PRIMARY KEY,
  operation_type TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
