#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${BASE_URL:-http://127.0.0.1:8080}"
MARKET_ID="${MARKET_ID:-market-demo}"
OUTCOME_ID="${OUTCOME_ID:-yes}"
USERS="${USERS:-20}"
RATE_PER_SEC="${RATE_PER_SEC:-8}"
CANCEL_PCT="${CANCEL_PCT:-25}"
ADMIN_TOKEN="${ADMIN_TOKEN:-admin-token}"
BOOTSTRAP_MARKET="${BOOTSTRAP_MARKET:-1}"

active_order_ids=()
active_order_users=()

placed=0
canceled=0
rejected=0
errors=0
loops=0

if [[ "${RATE_PER_SEC}" -le 0 ]]; then
  echo "RATE_PER_SEC must be > 0"
  exit 1
fi

if [[ "${USERS}" -le 0 ]]; then
  echo "USERS must be > 0"
  exit 1
fi

sleep_secs=$(awk "BEGIN { printf \"%.4f\", 1/${RATE_PER_SEC} }")

now_ms() {
  python3 - <<'PY'
import time
print(int(time.time()*1000))
PY
}

rand_user() {
  local idx=$(( (RANDOM % USERS) + 1 ))
  printf "user-%03d" "${idx}"
}

bootstrap_market() {
  local payload
  payload=$(cat <<JSON
{"market_id":"${MARKET_ID}","outcomes":["yes","no"],"sources":["synthetic"],"criteria":"synthetic test market","dispute_window_secs":0,"price_bands":{"min_tick":0,"max_tick":10000},"fee_config":{"taker_fee_bps":25}}
JSON
)

  local response
  response=$(curl -sS -X POST "${BASE_URL}/admin/markets" \
    -H "x-admin-token: ${ADMIN_TOKEN}" \
    -H "content-type: application/json" \
    -d "${payload}" || true)

  if [[ "${response}" == *"market already exists"* ]]; then
    echo "bootstrap: market already exists (${MARKET_ID})"
  elif [[ "${response}" == *"\"market\""* ]]; then
    echo "bootstrap: created market ${MARKET_ID}"
  else
    echo "bootstrap: response ${response}"
  fi
}

place_order() {
  local user_id side price qty ts order_id command_id payload response
  user_id=$(rand_user)
  side="buy"
  if (( RANDOM % 2 == 0 )); then
    side="sell"
  fi

  price=$(( 4500 + (RANDOM % 1001) ))
  qty=$(( 1 + (RANDOM % 30) ))
  ts=$(now_ms)
  order_id="ord-${ts}-${RANDOM}"
  command_id="cmd-place-${ts}-${RANDOM}"

  payload=$(cat <<JSON
{"command_id":"${command_id}","market_id":"${MARKET_ID}","outcome_id":"${OUTCOME_ID}","user_id":"${user_id}","order_id":"${order_id}","side":"${side}","order_type":"limit","limit_price":${price},"qty":${qty}}
JSON
)

  response=$(curl -sS -X POST "${BASE_URL}/orders" \
    -H "content-type: application/json" \
    -d "${payload}" || true)

  if [[ "${response}" == *"order_accepted"* ]]; then
    active_order_ids+=("${order_id}")
    active_order_users+=("${user_id}")
    placed=$((placed + 1))
  elif [[ "${response}" == *"order_rejected"* ]]; then
    rejected=$((rejected + 1))
  else
    errors=$((errors + 1))
  fi
}

cancel_order() {
  local count idx order_id user_id ts command_id payload response
  count=${#active_order_ids[@]}
  if (( count == 0 )); then
    place_order
    return
  fi

  idx=$(( RANDOM % count ))
  order_id="${active_order_ids[$idx]}"
  user_id="${active_order_users[$idx]}"
  ts=$(now_ms)
  command_id="cmd-cancel-${ts}-${RANDOM}"

  payload=$(cat <<JSON
{"command_id":"${command_id}","market_id":"${MARKET_ID}","outcome_id":"${OUTCOME_ID}","user_id":"${user_id}"}
JSON
)

  response=$(curl -sS -X DELETE "${BASE_URL}/orders/${order_id}" \
    -H "content-type: application/json" \
    -d "${payload}" || true)

  if [[ "${response}" == *"order_canceled"* ]]; then
    unset 'active_order_ids[idx]'
    unset 'active_order_users[idx]'
    active_order_ids=("${active_order_ids[@]}")
    active_order_users=("${active_order_users[@]}")
    canceled=$((canceled + 1))
  elif [[ "${response}" == *"order_rejected"* ]]; then
    rejected=$((rejected + 1))
  else
    errors=$((errors + 1))
  fi
}

print_stats() {
  local open_count
  open_count=${#active_order_ids[@]}
  printf "placed=%d canceled=%d rejected=%d errors=%d open_orders=%d\n" \
    "${placed}" "${canceled}" "${rejected}" "${errors}" "${open_count}"
}

on_exit() {
  echo
  echo "traffic stopped"
  print_stats
}
trap on_exit EXIT

echo "base_url=${BASE_URL} market=${MARKET_ID} outcome=${OUTCOME_ID} users=${USERS} rate_per_sec=${RATE_PER_SEC}"

if [[ "${BOOTSTRAP_MARKET}" == "1" ]]; then
  bootstrap_market
fi

while true; do
  if (( RANDOM % 100 < CANCEL_PCT )); then
    cancel_order
  else
    place_order
  fi

  loops=$((loops + 1))
  if (( loops % RATE_PER_SEC == 0 )); then
    print_stats
  fi

  sleep "${sleep_secs}"
done
