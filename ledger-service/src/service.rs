use std::sync::Arc;

use anyhow::Context;
use sqlx::{PgPool, Postgres, Row, Transaction};
use tonic::{Request, Response, Status};
use tracing::info;

use crate::logic::{checked_notional, reduce_long_first, taker_fee_czk};
use crate::pb::ledger_service_server::LedgerService;
use crate::pb::{
    AdjustReservationRequest, AdjustReservationResponse, ApplyFillRequest, ApplyFillResponse,
    GetBalancesRequest, GetBalancesResponse, GetPositionsRequest, GetPositionsResponse, Position,
    ReleaseReservationRequest, ReleaseReservationResponse, ReserveForOrderRequest,
    ReserveForOrderResponse, SettleMarketRequest, SettleMarketResponse, Side,
};

const FACE_CZK: i64 = 10_000;

#[derive(Clone)]
pub struct LedgerGrpcService {
    pool: Arc<PgPool>,
}

impl LedgerGrpcService {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool: Arc::new(pool),
        }
    }

    async fn begin_serializable(&self) -> Result<Transaction<'_, Postgres>, Status> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(internal_error("begin tx failed"))?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *tx)
            .await
            .map_err(internal_error("set isolation failed"))?;
        Ok(tx)
    }
}

fn internal_error(msg: &'static str) -> impl FnOnce(sqlx::Error) -> Status {
    move |e| Status::internal(format!("{msg}: {e}"))
}

async fn mark_command(
    tx: &mut Transaction<'_, Postgres>,
    command_id: &str,
    operation_type: &str,
) -> Result<bool, Status> {
    let result = sqlx::query(
        "INSERT INTO processed_commands(command_id, operation_type) VALUES($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(command_id)
    .bind(operation_type)
    .execute(&mut **tx)
    .await
    .map_err(internal_error("processed_commands insert failed"))?;

    Ok(result.rows_affected() > 0)
}

async fn ensure_wallet(tx: &mut Transaction<'_, Postgres>, user_id: &str) -> Result<(), Status> {
    sqlx::query("INSERT INTO wallets(user_id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(user_id)
        .execute(&mut **tx)
        .await
        .map_err(internal_error("wallet upsert failed"))?;
    Ok(())
}

async fn lock_wallet(
    tx: &mut Transaction<'_, Postgres>,
    user_id: &str,
) -> Result<(i64, i64), Status> {
    let row = sqlx::query(
        "SELECT available_czk, reserved_czk FROM wallets WHERE user_id = $1 FOR UPDATE",
    )
    .bind(user_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(internal_error("wallet lock failed"))?;
    Ok((row.get::<i64, _>(0), row.get::<i64, _>(1)))
}

async fn update_wallet(
    tx: &mut Transaction<'_, Postgres>,
    user_id: &str,
    available_czk: i64,
    reserved_czk: i64,
) -> Result<(), Status> {
    if available_czk < 0 || reserved_czk < 0 {
        return Err(Status::failed_precondition("negative wallet state"));
    }

    sqlx::query("UPDATE wallets SET available_czk = $2, reserved_czk = $3 WHERE user_id = $1")
        .bind(user_id)
        .bind(available_czk)
        .bind(reserved_czk)
        .execute(&mut **tx)
        .await
        .map_err(internal_error("wallet update failed"))?;

    Ok(())
}

async fn consume_reservation(
    tx: &mut Transaction<'_, Postgres>,
    reservation_id: &str,
    consume_czk: i64,
) -> Result<(), Status> {
    if reservation_id.is_empty() || consume_czk == 0 {
        return Ok(());
    }

    let row = sqlx::query(
        "SELECT amount_czk, consumed_czk, status FROM reservations WHERE reservation_id = $1 FOR UPDATE",
    )
    .bind(reservation_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(internal_error("reservation lock failed"))?;

    let Some(row) = row else {
        return Err(Status::not_found("reservation not found"));
    };

    let amount = row.get::<i64, _>(0);
    let consumed = row.get::<i64, _>(1);
    let status = row.get::<String, _>(2);

    if status != "active" {
        return Err(Status::failed_precondition("reservation not active"));
    }

    let remaining = amount - consumed;
    if remaining < consume_czk {
        return Err(Status::failed_precondition("reservation insufficient"));
    }

    let new_consumed = consumed + consume_czk;
    let new_status = if new_consumed == amount {
        "consumed"
    } else {
        "active"
    };

    sqlx::query("UPDATE reservations SET consumed_czk = $2, status = $3 WHERE reservation_id = $1")
        .bind(reservation_id)
        .bind(new_consumed)
        .bind(new_status)
        .execute(&mut **tx)
        .await
        .map_err(internal_error("reservation consume update failed"))?;

    Ok(())
}

async fn debit_wallet_reserved_first(
    tx: &mut Transaction<'_, Postgres>,
    user_id: &str,
    amount_czk: i64,
) -> Result<(), Status> {
    if amount_czk == 0 {
        return Ok(());
    }

    let (available, reserved) = lock_wallet(tx, user_id).await?;
    let total = available
        .checked_add(reserved)
        .ok_or_else(|| Status::internal("wallet overflow"))?;

    if total < amount_czk {
        return Err(Status::failed_precondition("insufficient funds"));
    }

    let consume_reserved = reserved.min(amount_czk);
    let from_available = amount_czk - consume_reserved;

    update_wallet(
        tx,
        user_id,
        available - from_available,
        reserved - consume_reserved,
    )
    .await
}

async fn credit_wallet_available(
    tx: &mut Transaction<'_, Postgres>,
    user_id: &str,
    amount_czk: i64,
) -> Result<(), Status> {
    if amount_czk == 0 {
        return Ok(());
    }

    let (available, reserved) = lock_wallet(tx, user_id).await?;
    let new_available = available
        .checked_add(amount_czk)
        .ok_or_else(|| Status::internal("wallet overflow"))?;
    update_wallet(tx, user_id, new_available, reserved).await
}

async fn upsert_position_delta(
    tx: &mut Transaction<'_, Postgres>,
    user_id: &str,
    market_id: &str,
    outcome_id: &str,
    long_delta: i64,
    short_delta: i64,
) -> Result<(), Status> {
    sqlx::query(
        r#"
        INSERT INTO positions(user_id, market_id, outcome_id, long_qty, short_qty)
        VALUES ($1, $2, $3, GREATEST($4, 0), GREATEST($5, 0))
        ON CONFLICT (user_id, market_id, outcome_id)
        DO UPDATE SET
          long_qty = positions.long_qty + $4,
          short_qty = positions.short_qty + $5
        "#,
    )
    .bind(user_id)
    .bind(market_id)
    .bind(outcome_id)
    .bind(long_delta)
    .bind(short_delta)
    .execute(&mut **tx)
    .await
    .map_err(internal_error("position upsert failed"))?;

    Ok(())
}

async fn apply_sell_position(
    tx: &mut Transaction<'_, Postgres>,
    user_id: &str,
    market_id: &str,
    outcome_id: &str,
    qty: i64,
) -> Result<(), Status> {
    let row = sqlx::query(
        "SELECT long_qty, short_qty FROM positions WHERE user_id=$1 AND market_id=$2 AND outcome_id=$3 FOR UPDATE",
    )
    .bind(user_id)
    .bind(market_id)
    .bind(outcome_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(internal_error("position lock failed"))?;

    let (long, short) = row
        .map(|r| (r.get::<i64, _>(0), r.get::<i64, _>(1)))
        .unwrap_or((0, 0));

    let (new_long, short_add) = reduce_long_first(long, qty);
    let new_short = short
        .checked_add(short_add)
        .ok_or_else(|| Status::internal("position overflow"))?;

    sqlx::query(
        r#"
        INSERT INTO positions(user_id, market_id, outcome_id, long_qty, short_qty)
        VALUES($1,$2,$3,$4,$5)
        ON CONFLICT (user_id, market_id, outcome_id)
        DO UPDATE SET long_qty = $4, short_qty = $5
        "#,
    )
    .bind(user_id)
    .bind(market_id)
    .bind(outcome_id)
    .bind(new_long)
    .bind(new_short)
    .execute(&mut **tx)
    .await
    .map_err(internal_error("position sell update failed"))?;

    Ok(())
}

async fn settle_user_chunk(
    tx: &mut Transaction<'_, Postgres>,
    market_id: &str,
    winning_outcome_id: &str,
    after_user_id: &str,
    chunk_size: i64,
) -> Result<(i64, Option<String>), Status> {
    let users = sqlx::query(
        r#"
        SELECT DISTINCT user_id
        FROM positions
        WHERE market_id = $1 AND user_id > $2
        ORDER BY user_id
        LIMIT $3
        "#,
    )
    .bind(market_id)
    .bind(after_user_id)
    .bind(chunk_size)
    .fetch_all(&mut **tx)
    .await
    .map_err(internal_error("settlement users query failed"))?;

    if users.is_empty() {
        return Ok((0, None));
    }

    let mut processed_users = 0_i64;
    let mut last_user_id = None;
    for row in users {
        let user_id: String = row.get(0);
        ensure_wallet(tx, &user_id).await?;

        let sums = sqlx::query(
            r#"
            SELECT
              COALESCE(SUM(CASE WHEN outcome_id = $2 THEN long_qty ELSE 0 END), 0) AS winning_long_qty,
              COALESCE(SUM(CASE WHEN outcome_id = $2 THEN short_qty ELSE 0 END), 0) AS winning_short_qty
            FROM positions
            WHERE market_id = $1 AND user_id = $3
            "#,
        )
        .bind(market_id)
        .bind(winning_outcome_id)
        .bind(&user_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(internal_error("settlement aggregate query failed"))?;

        let winning_long_qty: i64 = sums.get(0);
        let winning_short_qty: i64 = sums.get(1);

        let payout_czk = winning_long_qty
            .checked_mul(FACE_CZK)
            .ok_or_else(|| Status::internal("payout overflow"))?;
        let debit_czk = winning_short_qty
            .checked_mul(FACE_CZK)
            .ok_or_else(|| Status::internal("debit overflow"))?;

        if payout_czk > 0 {
            credit_wallet_available(tx, &user_id, payout_czk).await?;
        }
        if debit_czk > 0 {
            debit_wallet_reserved_first(tx, &user_id, debit_czk).await?;
        }

        sqlx::query(
            "UPDATE positions SET long_qty = 0, short_qty = 0 WHERE market_id = $1 AND user_id = $2",
        )
        .bind(market_id)
        .bind(&user_id)
        .execute(&mut **tx)
        .await
        .map_err(internal_error("position settlement update failed"))?;

        processed_users += 1;
        last_user_id = Some(user_id);
    }

    Ok((processed_users, last_user_id))
}

async fn release_market_reservation_chunk(
    tx: &mut Transaction<'_, Postgres>,
    market_id: &str,
    after_reservation_id: &str,
    chunk_size: i64,
) -> Result<(i64, Option<String>), Status> {
    let rows = sqlx::query(
        r#"
        SELECT reservation_id, user_id, amount_czk, consumed_czk
        FROM reservations
        WHERE market_id = $1
          AND status = 'active'
          AND reservation_id > $2
        ORDER BY reservation_id
        LIMIT $3
        FOR UPDATE
        "#,
    )
    .bind(market_id)
    .bind(after_reservation_id)
    .bind(chunk_size)
    .fetch_all(&mut **tx)
    .await
    .map_err(internal_error("reservation release query failed"))?;

    if rows.is_empty() {
        return Ok((0, None));
    }

    let mut released = 0_i64;
    let mut last_reservation_id = None;
    for row in rows {
        let reservation_id: String = row.get(0);
        let user_id: String = row.get(1);
        let amount_czk: i64 = row.get(2);
        let consumed_czk: i64 = row.get(3);
        let releasable = amount_czk - consumed_czk;
        if releasable < 0 {
            return Err(Status::internal("reservation invariant violated"));
        }

        if releasable > 0 {
            let (available, reserved) = lock_wallet(tx, &user_id).await?;
            if reserved < releasable {
                return Err(Status::failed_precondition("reserved wallet underflow"));
            }
            update_wallet(tx, &user_id, available + releasable, reserved - releasable).await?;
        }

        sqlx::query("UPDATE reservations SET status='released' WHERE reservation_id=$1")
            .bind(&reservation_id)
            .execute(&mut **tx)
            .await
            .map_err(internal_error("reservation release update failed"))?;

        released += 1;
        last_reservation_id = Some(reservation_id);
    }

    Ok((released, last_reservation_id))
}

#[tonic::async_trait]
impl LedgerService for LedgerGrpcService {
    async fn reserve_for_order(
        &self,
        request: Request<ReserveForOrderRequest>,
    ) -> Result<Response<ReserveForOrderResponse>, Status> {
        let req = request.into_inner();
        if req.amount_czk < 0 {
            return Err(Status::invalid_argument("amount_czk must be non-negative"));
        }

        let mut tx = self.begin_serializable().await?;
        if !mark_command(&mut tx, &req.command_id, "reserve_for_order").await? {
            tx.commit().await.map_err(internal_error("commit failed"))?;
            return Ok(Response::new(ReserveForOrderResponse {
                ok: true,
                reason: "idempotent replay".into(),
            }));
        }

        ensure_wallet(&mut tx, &req.user_id).await?;
        let (available, reserved) = lock_wallet(&mut tx, &req.user_id).await?;
        if available < req.amount_czk {
            tx.rollback()
                .await
                .map_err(internal_error("rollback failed"))?;
            return Ok(Response::new(ReserveForOrderResponse {
                ok: false,
                reason: "insufficient available funds".into(),
            }));
        }

        update_wallet(
            &mut tx,
            &req.user_id,
            available - req.amount_czk,
            reserved + req.amount_czk,
        )
        .await?;

        let kind = format!("{:?}", req.kind).to_lowercase();
        let insert = sqlx::query(
            r#"
            INSERT INTO reservations(
              reservation_id,
              user_id,
              order_id,
              kind,
              amount_czk,
              consumed_czk,
              status,
              market_id,
              outcome_id
            )
            VALUES($1, $2, $3, $4, $5, 0, 'active', $6, $7)
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(&req.reservation_id)
        .bind(&req.user_id)
        .bind(&req.order_id)
        .bind(kind)
        .bind(req.amount_czk)
        .bind(&req.market_id)
        .bind(&req.outcome_id)
        .execute(&mut *tx)
        .await
        .map_err(internal_error("reservation insert failed"))?;

        if insert.rows_affected() == 0 {
            tx.rollback()
                .await
                .map_err(internal_error("rollback failed"))?;
            return Ok(Response::new(ReserveForOrderResponse {
                ok: false,
                reason: "reservation conflict".into(),
            }));
        }

        tx.commit().await.map_err(internal_error("commit failed"))?;
        info!(
            "audit reserve_for_order user={} amount={}",
            req.user_id, req.amount_czk
        );

        Ok(Response::new(ReserveForOrderResponse {
            ok: true,
            reason: "".into(),
        }))
    }

    async fn adjust_reservation(
        &self,
        request: Request<AdjustReservationRequest>,
    ) -> Result<Response<AdjustReservationResponse>, Status> {
        let req = request.into_inner();
        let mut tx = self.begin_serializable().await?;

        if !mark_command(&mut tx, &req.command_id, "adjust_reservation").await? {
            tx.commit().await.map_err(internal_error("commit failed"))?;
            return Ok(Response::new(AdjustReservationResponse {
                ok: true,
                reason: "idempotent replay".into(),
            }));
        }

        let row = sqlx::query(
            "SELECT user_id, amount_czk, consumed_czk, status FROM reservations WHERE reservation_id = $1 FOR UPDATE",
        )
        .bind(&req.reservation_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal_error("reservation lock failed"))?;

        let Some(row) = row else {
            tx.rollback()
                .await
                .map_err(internal_error("rollback failed"))?;
            return Ok(Response::new(AdjustReservationResponse {
                ok: false,
                reason: "reservation not found".into(),
            }));
        };

        let user_id = row.get::<String, _>(0);
        let amount = row.get::<i64, _>(1);
        let consumed = row.get::<i64, _>(2);
        let status = row.get::<String, _>(3);

        if status != "active" {
            tx.rollback()
                .await
                .map_err(internal_error("rollback failed"))?;
            return Ok(Response::new(AdjustReservationResponse {
                ok: false,
                reason: "reservation not active".into(),
            }));
        }

        let new_amount = amount + req.delta_czk;
        if new_amount < consumed || new_amount < 0 {
            tx.rollback()
                .await
                .map_err(internal_error("rollback failed"))?;
            return Ok(Response::new(AdjustReservationResponse {
                ok: false,
                reason: "invalid delta".into(),
            }));
        }

        let (available, reserved) = lock_wallet(&mut tx, &user_id).await?;
        if req.delta_czk > 0 {
            if available < req.delta_czk {
                tx.rollback()
                    .await
                    .map_err(internal_error("rollback failed"))?;
                return Ok(Response::new(AdjustReservationResponse {
                    ok: false,
                    reason: "insufficient available funds".into(),
                }));
            }
            update_wallet(
                &mut tx,
                &user_id,
                available - req.delta_czk,
                reserved + req.delta_czk,
            )
            .await?;
        } else if req.delta_czk < 0 {
            let release = -req.delta_czk;
            if reserved < release {
                tx.rollback()
                    .await
                    .map_err(internal_error("rollback failed"))?;
                return Ok(Response::new(AdjustReservationResponse {
                    ok: false,
                    reason: "insufficient reserved funds".into(),
                }));
            }
            update_wallet(&mut tx, &user_id, available + release, reserved - release).await?;
        }

        let new_status = if new_amount == consumed {
            "consumed"
        } else {
            "active"
        };

        sqlx::query(
            "UPDATE reservations SET amount_czk = $2, status = $3 WHERE reservation_id = $1",
        )
        .bind(&req.reservation_id)
        .bind(new_amount)
        .bind(new_status)
        .execute(&mut *tx)
        .await
        .map_err(internal_error("reservation update failed"))?;

        tx.commit().await.map_err(internal_error("commit failed"))?;
        info!(
            "audit adjust_reservation reservation={} delta={}",
            req.reservation_id, req.delta_czk
        );

        Ok(Response::new(AdjustReservationResponse {
            ok: true,
            reason: "".into(),
        }))
    }

    async fn release_reservation(
        &self,
        request: Request<ReleaseReservationRequest>,
    ) -> Result<Response<ReleaseReservationResponse>, Status> {
        let req = request.into_inner();
        let mut tx = self.begin_serializable().await?;

        if !mark_command(&mut tx, &req.command_id, "release_reservation").await? {
            tx.commit().await.map_err(internal_error("commit failed"))?;
            return Ok(Response::new(ReleaseReservationResponse {
                ok: true,
                reason: "idempotent replay".into(),
            }));
        }

        let row = sqlx::query(
            "SELECT user_id, amount_czk, consumed_czk, status FROM reservations WHERE reservation_id = $1 FOR UPDATE",
        )
        .bind(&req.reservation_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal_error("reservation lock failed"))?;

        let Some(row) = row else {
            tx.rollback()
                .await
                .map_err(internal_error("rollback failed"))?;
            return Ok(Response::new(ReleaseReservationResponse {
                ok: false,
                reason: "reservation not found".into(),
            }));
        };

        let user_id = row.get::<String, _>(0);
        let amount = row.get::<i64, _>(1);
        let consumed = row.get::<i64, _>(2);
        let status = row.get::<String, _>(3);

        if status == "released" {
            tx.commit().await.map_err(internal_error("commit failed"))?;
            return Ok(Response::new(ReleaseReservationResponse {
                ok: true,
                reason: "already released".into(),
            }));
        }

        let releasable = amount - consumed;
        if releasable < 0 {
            return Err(Status::internal("reservation invariant violated"));
        }

        let (available, reserved) = lock_wallet(&mut tx, &user_id).await?;
        if reserved < releasable {
            tx.rollback()
                .await
                .map_err(internal_error("rollback failed"))?;
            return Ok(Response::new(ReleaseReservationResponse {
                ok: false,
                reason: "reserved wallet underflow".into(),
            }));
        }

        update_wallet(
            &mut tx,
            &user_id,
            available + releasable,
            reserved - releasable,
        )
        .await?;

        sqlx::query("UPDATE reservations SET status='released' WHERE reservation_id=$1")
            .bind(&req.reservation_id)
            .execute(&mut *tx)
            .await
            .map_err(internal_error("reservation release failed"))?;

        tx.commit().await.map_err(internal_error("commit failed"))?;
        info!(
            "audit release_reservation reservation={}",
            req.reservation_id
        );

        Ok(Response::new(ReleaseReservationResponse {
            ok: true,
            reason: "".into(),
        }))
    }

    async fn apply_fill(
        &self,
        request: Request<ApplyFillRequest>,
    ) -> Result<Response<ApplyFillResponse>, Status> {
        let req = request.into_inner();
        if req.qty <= 0 || req.price_czk < 0 || req.fee_bps < 0 {
            return Err(Status::invalid_argument("qty/price/fee_bps invalid"));
        }

        let mut tx = self.begin_serializable().await?;

        let existing = sqlx::query("SELECT notional_czk, fee_czk FROM fills WHERE fill_id = $1")
            .bind(&req.fill_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal_error("fill lookup failed"))?;

        if let Some(row) = existing {
            tx.commit().await.map_err(internal_error("commit failed"))?;
            return Ok(Response::new(ApplyFillResponse {
                ok: true,
                reason: "idempotent replay".into(),
                notional_czk: row.get(0),
                fee_czk: row.get(1),
            }));
        }

        let notional_czk = checked_notional(req.qty, req.price_czk)
            .context("notional overflow")
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let fee_czk = taker_fee_czk(notional_czk, req.fee_bps)
            .context("fee overflow")
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

        let maker = req
            .maker
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("maker is required"))?;
        let taker = req
            .taker
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("taker is required"))?;
        let taker_is_buy = req.taker_side == Side::Buy as i32;
        let (buyer, seller) = if taker_is_buy {
            (taker, maker)
        } else {
            (maker, taker)
        };

        ensure_wallet(&mut tx, &buyer.user_id).await?;
        ensure_wallet(&mut tx, &seller.user_id).await?;

        // Funds transfer: buyer pays notional, seller receives notional.
        debit_wallet_reserved_first(&mut tx, &buyer.user_id, notional_czk).await?;
        credit_wallet_available(&mut tx, &seller.user_id, notional_czk).await?;

        // Fee is charged to taker only.
        debit_wallet_reserved_first(&mut tx, &taker.user_id, fee_czk).await?;

        let buyer_consumption = if taker_is_buy {
            notional_czk + fee_czk
        } else {
            notional_czk
        };
        consume_reservation(&mut tx, &buyer.reservation_id, buyer_consumption).await?;

        if !taker_is_buy {
            consume_reservation(&mut tx, &taker.reservation_id, fee_czk).await?;
        }

        upsert_position_delta(
            &mut tx,
            &buyer.user_id,
            &req.market_id,
            &req.outcome_id,
            req.qty,
            0,
        )
        .await?;

        apply_sell_position(
            &mut tx,
            &seller.user_id,
            &req.market_id,
            &req.outcome_id,
            req.qty,
        )
        .await?;

        sqlx::query(
            r#"
            INSERT INTO fills(fill_id, maker_user_id, taker_user_id, maker_order_id, taker_order_id, qty, price_czk, notional_czk, fee_czk)
            VALUES($1, $2, $3, $4, $5, $6, $7, $8, $9)
            "#,
        )
        .bind(&req.fill_id)
        .bind(&maker.user_id)
        .bind(&taker.user_id)
        .bind(&maker.order_id)
        .bind(&taker.order_id)
        .bind(req.qty)
        .bind(req.price_czk)
        .bind(notional_czk)
        .bind(fee_czk)
        .execute(&mut *tx)
        .await
        .map_err(internal_error("fill insert failed"))?;

        tx.commit().await.map_err(internal_error("commit failed"))?;
        info!(
            "audit apply_fill fill_id={} notional={} fee={}",
            req.fill_id, notional_czk, fee_czk
        );

        Ok(Response::new(ApplyFillResponse {
            ok: true,
            reason: "".into(),
            notional_czk,
            fee_czk,
        }))
    }

    async fn settle_market(
        &self,
        request: Request<SettleMarketRequest>,
    ) -> Result<Response<SettleMarketResponse>, Status> {
        let req = request.into_inner();
        if req.market_id.trim().is_empty() || req.winning_outcome_id.trim().is_empty() {
            return Err(Status::invalid_argument(
                "market_id and winning_outcome_id are required",
            ));
        }
        if req.idempotency_key.trim().is_empty() {
            return Err(Status::invalid_argument("idempotency_key is required"));
        }
        let chunk_size = i64::from(req.chunk_size.clamp(1, 5_000));

        loop {
            let mut tx = self.begin_serializable().await?;

            let existing = sqlx::query(
                r#"
                SELECT idempotency_key, status, last_user_id, last_reservation_id, processed_users, released_reservations
                FROM settlement_runs
                WHERE market_id = $1
                FOR UPDATE
                "#,
            )
            .bind(&req.market_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal_error("settlement run lookup failed"))?;

            let (last_user_id, last_reservation_id, mut processed_users, mut released_reservations) =
                if let Some(row) = existing {
                    let existing_key: String = row.get(0);
                    let status: String = row.get(1);
                    let last_user_id: String = row.get(2);
                    let last_reservation_id: String = row.get(3);
                    let processed_users: i64 = row.get(4);
                    let released_reservations: i64 = row.get(5);

                    if existing_key != req.idempotency_key {
                        tx.commit().await.map_err(internal_error("commit failed"))?;
                        return Ok(Response::new(SettleMarketResponse {
                            ok: false,
                            reason: "market already settled with different idempotency key".into(),
                            idempotent: false,
                            processed_users,
                            released_reservations,
                        }));
                    }

                    if status == "completed" {
                        tx.commit().await.map_err(internal_error("commit failed"))?;
                        return Ok(Response::new(SettleMarketResponse {
                            ok: true,
                            reason: "idempotent replay".into(),
                            idempotent: true,
                            processed_users,
                            released_reservations,
                        }));
                    }

                    (
                        last_user_id,
                        last_reservation_id,
                        processed_users,
                        released_reservations,
                    )
                } else {
                    sqlx::query(
                        r#"
                        INSERT INTO settlement_runs(
                          market_id,
                          idempotency_key,
                          winning_outcome_id,
                          status,
                          last_user_id,
                          last_reservation_id,
                          processed_users,
                          released_reservations
                        )
                        VALUES($1, $2, $3, 'in_progress', '', '', 0, 0)
                        "#,
                    )
                    .bind(&req.market_id)
                    .bind(&req.idempotency_key)
                    .bind(&req.winning_outcome_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(internal_error("insert settlement run failed"))?;
                    ("".to_string(), "".to_string(), 0_i64, 0_i64)
                };

            let (processed_chunk, next_user) = settle_user_chunk(
                &mut tx,
                &req.market_id,
                &req.winning_outcome_id,
                &last_user_id,
                chunk_size,
            )
            .await?;
            processed_users += processed_chunk;

            let (released_chunk, next_reservation) = release_market_reservation_chunk(
                &mut tx,
                &req.market_id,
                &last_reservation_id,
                chunk_size,
            )
            .await?;
            released_reservations += released_chunk;

            let new_last_user = next_user.unwrap_or(last_user_id);
            let new_last_reservation = next_reservation.unwrap_or(last_reservation_id);
            let completed = processed_chunk == 0 && released_chunk == 0;

            sqlx::query(
                r#"
                UPDATE settlement_runs
                SET
                  status = $2,
                  last_user_id = $3,
                  last_reservation_id = $4,
                  processed_users = $5,
                  released_reservations = $6,
                  winning_outcome_id = $7,
                  updated_at = NOW()
                WHERE market_id = $1
                "#,
            )
            .bind(&req.market_id)
            .bind(if completed {
                "completed"
            } else {
                "in_progress"
            })
            .bind(new_last_user)
            .bind(new_last_reservation)
            .bind(processed_users)
            .bind(released_reservations)
            .bind(&req.winning_outcome_id)
            .execute(&mut *tx)
            .await
            .map_err(internal_error("update settlement run failed"))?;

            tx.commit().await.map_err(internal_error("commit failed"))?;

            if completed {
                info!(
                    "audit settle_market market={} processed_users={} released_reservations={}",
                    req.market_id, processed_users, released_reservations
                );
                return Ok(Response::new(SettleMarketResponse {
                    ok: true,
                    reason: "".into(),
                    idempotent: false,
                    processed_users,
                    released_reservations,
                }));
            }
        }
    }

    async fn get_balances(
        &self,
        request: Request<GetBalancesRequest>,
    ) -> Result<Response<GetBalancesResponse>, Status> {
        let req = request.into_inner();
        let row = sqlx::query("SELECT available_czk, reserved_czk FROM wallets WHERE user_id = $1")
            .bind(req.user_id)
            .fetch_optional(&*self.pool)
            .await
            .map_err(internal_error("balance query failed"))?;

        let (available_czk, reserved_czk) =
            row.map(|r| (r.get(0), r.get(1))).unwrap_or((0_i64, 0_i64));

        Ok(Response::new(GetBalancesResponse {
            available_czk,
            reserved_czk,
        }))
    }

    async fn get_positions(
        &self,
        request: Request<GetPositionsRequest>,
    ) -> Result<Response<GetPositionsResponse>, Status> {
        let req = request.into_inner();
        let rows = sqlx::query(
            "SELECT market_id, outcome_id, long_qty, short_qty FROM positions WHERE user_id = $1 ORDER BY market_id, outcome_id",
        )
        .bind(req.user_id)
        .fetch_all(&*self.pool)
        .await
        .map_err(internal_error("positions query failed"))?;

        let positions = rows
            .into_iter()
            .map(|r| Position {
                market_id: r.get(0),
                outcome_id: r.get(1),
                long_qty: r.get(2),
                short_qty: r.get(3),
            })
            .collect();

        Ok(Response::new(GetPositionsResponse { positions }))
    }
}
