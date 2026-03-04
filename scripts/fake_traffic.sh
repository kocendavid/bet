#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${BASE_URL:-http://127.0.0.1:8080}"
MARKET_ID="${MARKET_ID:-market-demo}"
OUTCOME_ID="${OUTCOME_ID:-yes}"
USERS="${USERS:-20}"
USER_ID_MODE="${USER_ID_MODE:-namespaced_pool}"
USER_NAMESPACE="${USER_NAMESPACE:-run-$(date +%s)-$RANDOM}"
RATE_PER_SEC="${RATE_PER_SEC:-8}"
CANCEL_PCT="${CANCEL_PCT:-25}"
MAX_OPEN_PER_USER="${MAX_OPEN_PER_USER:-90}"
OPEN_ORDER_MARGIN="${OPEN_ORDER_MARGIN:-5}"
ADMIN_TOKEN="${ADMIN_TOKEN:-admin-token}"
BOOTSTRAP_MARKET="${BOOTSTRAP_MARKET:-1}"
SYNC_ACTIVE_ON_START="${SYNC_ACTIVE_ON_START:-1}"

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
  if [[ "${USER_ID_MODE}" == "random" ]]; then
    local ts
    ts=$(now_ms)
    printf "%s-user-%d-%d" "${USER_NAMESPACE}" "${ts}" "${RANDOM}"
    return
  fi

  local idx=$(( (RANDOM % USERS) + 1 ))
  printf "%s-user-%03d" "${USER_NAMESPACE}" "${idx}"
}

find_active_index_by_order_id() {
  local order_id=$1
  local i
  for (( i=0; i<${#active_order_ids[@]}; i++ )); do
    if [[ "${active_order_ids[$i]}" == "${order_id}" ]]; then
      echo "${i}"
      return 0
    fi
  done
  return 1
}

remove_active_index() {
  local idx=$1
  unset "active_order_ids[$idx]"
  unset "active_order_users[$idx]"
  active_order_ids=("${active_order_ids[@]}")
  active_order_users=("${active_order_users[@]}")
}

remove_active_order_id() {
  local order_id=$1
  local idx
  if idx=$(find_active_index_by_order_id "${order_id}"); then
    remove_active_index "${idx}"
    return 0
  fi
  return 1
}

add_active_order() {
  local order_id=$1
  local user_id=$2
  if find_active_index_by_order_id "${order_id}" >/dev/null; then
    return
  fi
  active_order_ids+=("${order_id}")
  active_order_users+=("${user_id}")
}

count_open_for_user() {
  local user_id=$1
  local i count=0
  for (( i=0; i<${#active_order_users[@]}; i++ )); do
    if [[ "${active_order_users[$i]}" == "${user_id}" ]]; then
      count=$((count + 1))
    fi
  done
  echo "${count}"
}

pick_cancel_index() {
  local target_user="${1:-}"
  local count i
  count=${#active_order_ids[@]}
  if (( count == 0 )); then
    return 1
  fi

  if [[ -z "${target_user}" ]]; then
    echo $(( RANDOM % count ))
    return 0
  fi

  local matching=()
  for (( i=0; i<count; i++ )); do
    if [[ "${active_order_users[$i]}" == "${target_user}" ]]; then
      matching+=("${i}")
    fi
  done

  if (( ${#matching[@]} == 0 )); then
    return 1
  fi

  echo "${matching[$(( RANDOM % ${#matching[@]} ))]}"
}

extract_events() {
  python3 - <<'PY'
import json
import sys

raw = sys.stdin.read().strip()
try:
    parsed = json.loads(raw) if raw else {}
except Exception:
    print("parse_error=1")
    raise SystemExit(0)

events = parsed.get("events") if isinstance(parsed, dict) else []
if not isinstance(events, list):
    events = []

for ev in events:
    if not isinstance(ev, dict):
        continue
    et = ev.get("type")
    oid = ev.get("order_id")
    if et == "order_accepted" and isinstance(oid, str):
        print(f"accepted={oid}")
    elif et == "order_rested" and isinstance(oid, str):
        print(f"rested={oid}")
    elif et in ("order_canceled", "order_filled") and isinstance(oid, str):
        print(f"terminal={oid}")
    elif et == "order_rejected":
        if isinstance(oid, str):
            print(f"rejected_order={oid}")
        reason = ev.get("reason", "")
        if not isinstance(reason, str):
            reason = str(reason)
        print(f"reject_reason={reason}")
PY
}

sync_active_orders_from_book() {
  local response
  response=$(curl -sS "${BASE_URL}/books/${MARKET_ID}/${OUTCOME_ID}" || true)
  if [[ "${response}" != *"\"open_orders\""* ]]; then
    echo "sync_active_orders: unable to fetch book"
    return 1
  fi

  mapfile -t parsed < <(python3 - <<'PY' <<<"${response}"
import json
import sys

raw = sys.stdin.read().strip()
try:
    parsed = json.loads(raw) if raw else {}
except Exception:
    raise SystemExit(1)

orders = parsed.get("open_orders") or []
for o in orders:
    oid = o.get("order_id")
    uid = o.get("user_id")
    if isinstance(oid, str) and isinstance(uid, str):
        print(f"{oid}\t{uid}")
PY
  )

  active_order_ids=()
  active_order_users=()
  local line oid uid
  for line in "${parsed[@]}"; do
    oid="${line%%$'\t'*}"
    uid="${line#*$'\t'}"
    if [[ -n "${oid}" && -n "${uid}" ]]; then
      active_order_ids+=("${oid}")
      active_order_users+=("${uid}")
    fi
  done
  echo "sync_active_orders: loaded ${#active_order_ids[@]} open orders from book"
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
  local user_id side price qty ts order_id client_order_id command_id payload response
  local user_open local_limit
  local rested_order_id=""
  user_id=$(rand_user)

  local_limit=$(( MAX_OPEN_PER_USER - OPEN_ORDER_MARGIN ))
  if (( MAX_OPEN_PER_USER > 0 )); then
    if (( local_limit < 1 )); then
      local_limit=1
    fi
    user_open=$(count_open_for_user "${user_id}")
    if (( user_open >= local_limit )); then
      if cancel_order "${user_id}"; then
        return
      fi
    fi
  fi

  side="buy"
  if (( RANDOM % 2 == 0 )); then
    side="sell"
  fi

  price=$(( 4500 + (RANDOM % 1001) ))
  qty=$(( 1 + (RANDOM % 30) ))
  ts=$(now_ms)
  order_id="ord-${ts}-${RANDOM}"
  client_order_id="00000000-0000-4000-8000-$(printf "%012d" $(( (ts + RANDOM) % 1000000000000 )))"
  command_id="cmd-place-${ts}-${RANDOM}"

  payload=$(cat <<JSON
{"command_id":"${command_id}","market_id":"${MARKET_ID}","outcome_id":"${OUTCOME_ID}","user_id":"${user_id}","client_order_id":"${client_order_id}","order_id":"${order_id}","side":"${side}","order_type":"limit","limit_price":${price},"qty":${qty}}
JSON
)

  response=$(curl -sS -X POST "${BASE_URL}/orders" \
    -H "content-type: application/json" \
    -d "${payload}" || true)

  if [[ "${response}" != *"\"events\""* ]]; then
    errors=$((errors + 1))
    return
  fi

  while IFS= read -r entry; do
    case "${entry}" in
      rested=*)
        rested_order_id="${entry#rested=}"
        ;;
      terminal=*)
        remove_active_order_id "${entry#terminal=}" >/dev/null || true
        ;;
      reject_reason=*)
        rejected=$((rejected + 1))
        ;;
      parse_error=1)
        errors=$((errors + 1))
        return
        ;;
    esac
  done < <(extract_events <<<"${response}")

  if [[ -n "${rested_order_id}" ]]; then
    add_active_order "${rested_order_id}" "${user_id}"
  fi

  if [[ "${response}" == *"order_accepted"* ]]; then
    placed=$((placed + 1))
  elif [[ "${response}" != *"order_rejected"* ]]; then
    errors=$((errors + 1))
  fi
}

cancel_order() {
  local target_user="${1:-}"
  local idx order_id user_id ts command_id payload response
  local reject_reason=""
  if ! idx=$(pick_cancel_index "${target_user}"); then
    return 1
  fi

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

  if [[ "${response}" != *"\"events\""* ]]; then
    errors=$((errors + 1))
    return 1
  fi

  while IFS= read -r entry; do
    case "${entry}" in
      terminal=*)
        remove_active_order_id "${entry#terminal=}" >/dev/null || true
        ;;
      reject_reason=*)
        reject_reason="${entry#reject_reason=}"
        ;;
      parse_error=1)
        errors=$((errors + 1))
        return 1
        ;;
    esac
  done < <(extract_events <<<"${response}")

  if [[ "${response}" == *"order_canceled"* ]]; then
    remove_active_order_id "${order_id}" >/dev/null || true
    canceled=$((canceled + 1))
  elif [[ "${response}" == *"order_rejected"* ]]; then
    if [[ "${reject_reason}" == *"not found"* || "${reject_reason}" == *"terminal"* ]]; then
      remove_active_order_id "${order_id}" >/dev/null || true
    fi
    rejected=$((rejected + 1))
  else
    errors=$((errors + 1))
    return 1
  fi

  return 0
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

echo "base_url=${BASE_URL} market=${MARKET_ID} outcome=${OUTCOME_ID} users=${USERS} user_id_mode=${USER_ID_MODE} user_namespace=${USER_NAMESPACE} rate_per_sec=${RATE_PER_SEC} cancel_pct=${CANCEL_PCT} max_open_per_user=${MAX_OPEN_PER_USER} margin=${OPEN_ORDER_MARGIN}"

if [[ "${BOOTSTRAP_MARKET}" == "1" ]]; then
  bootstrap_market
fi

if [[ "${SYNC_ACTIVE_ON_START}" == "1" ]]; then
  sync_active_orders_from_book || true
fi

while true; do
  if (( RANDOM % 100 < CANCEL_PCT )); then
    if ! cancel_order; then
      place_order
    fi
  else
    place_order
  fi

  loops=$((loops + 1))
  if (( loops % RATE_PER_SEC == 0 )); then
    print_stats
  fi

  sleep "${sleep_secs}"
done
