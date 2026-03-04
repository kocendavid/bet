# Railway Deployment (Matcher Dashboard)

This repo now includes a root `Dockerfile` and `railway.json` for Railway deployment.

## What gets deployed

- Service: `matcher-service`
- Default mode: `MATCHER_LEDGER_MODE=accept_all`
- HTTP bind: `0.0.0.0:$PORT` (Railway-provided)
- Healthcheck: `GET /admin/metrics`

## Deploy Steps

1. Push this repo to GitHub.
2. In Railway, create a new project from the repo.
3. Railway will detect `Dockerfile` and build it.
4. Deploy.

## Optional Railway Variables

You can set these in Railway service variables:

- `MATCHER_SHARD_COUNT` (default `2`)
- `MATCHER_WS_QUEUE_CAPACITY` (default `256`)
- `RISK_MAX_OPEN_ORDERS`
- `RISK_MAX_QTY_PER_ORDER`
- `RISK_MAX_NOTIONAL_PER_ORDER_CZK`
- `RISK_MAX_SHORT_EXPOSURE_CZK`
- `RISK_MIN_TICK`
- `RISK_MAX_TICK`
- `MATCHER_DATA_DIR` (defaults to `/app/matcher-data`)

## Verify After Deploy

- Dashboard: `https://<your-service>.up.railway.app/dashboard`
- Metrics: `https://<your-service>.up.railway.app/admin/metrics`
- Quality: `https://<your-service>.up.railway.app/admin/quality`

## Notes

- `accept_all` ledger mode is for demo/testing traffic. It does not provide real ledger persistence.
- For production ledger behavior, deploy `ledger-service` + Postgres separately and set:
  - `MATCHER_LEDGER_MODE=grpc`
  - `LEDGER_GRPC_ADDR=http://<ledger-service-host>:50051`
