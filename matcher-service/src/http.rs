use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::ledger::LedgerAdapter;
use crate::sharding::ShardRuntime;
use crate::types::{CancelOrder, Command, PlaceOrder};

pub type SharedRuntime<L> = Arc<ShardRuntime<L>>;

pub fn router<L: LedgerAdapter + Clone + 'static>(runtime: SharedRuntime<L>) -> Router {
    Router::new()
        .route("/orders", post(place_order::<L>))
        .route("/orders/:order_id", delete(cancel_order::<L>))
        .route("/books/:market_id/:outcome_id", get(get_book::<L>))
        .route("/admin/migrate", post(migrate_market::<L>))
        .route("/admin/metrics", get(get_metrics::<L>))
        .route("/admin/routing/:market_id", get(get_routing::<L>))
        .with_state(runtime)
}

async fn place_order<L: LedgerAdapter + Clone>(
    State(runtime): State<SharedRuntime<L>>,
    Json(req): Json<PlaceOrder>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let out = runtime
        .process(Command::Place(req))
        .await
        .map_err(internal_error)?;
    Ok(Json(serde_json::json!({
        "command_id": out.command_id,
        "events": out.events
    })))
}

#[derive(Debug, Deserialize)]
struct CancelBody {
    command_id: String,
    market_id: String,
    outcome_id: String,
    user_id: String,
}

async fn cancel_order<L: LedgerAdapter + Clone>(
    Path(order_id): Path<String>,
    State(runtime): State<SharedRuntime<L>>,
    Json(body): Json<CancelBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let cmd = CancelOrder {
        command_id: body.command_id,
        market_id: body.market_id,
        outcome_id: body.outcome_id,
        user_id: body.user_id,
        order_id,
    };

    let out = runtime
        .process(Command::Cancel(cmd))
        .await
        .map_err(internal_error)?;
    Ok(Json(serde_json::json!({
        "command_id": out.command_id,
        "events": out.events
    })))
}

async fn get_book<L: LedgerAdapter + Clone>(
    Path((market_id, outcome_id)): Path<(String, String)>,
    State(runtime): State<SharedRuntime<L>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    match runtime
        .get_book(market_id, outcome_id)
        .await
        .map_err(internal_error)?
    {
        Some(book) => Ok(Json(book)),
        None => Err((StatusCode::NOT_FOUND, "book not found".into())),
    }
}

#[derive(Debug, Deserialize)]
struct MigrateBody {
    market_id: String,
    outcome_id: String,
    target_shard: usize,
}

async fn migrate_market<L: LedgerAdapter + Clone>(
    State(runtime): State<SharedRuntime<L>>,
    Json(body): Json<MigrateBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    runtime
        .migrate_market(body.market_id, body.outcome_id, body.target_shard)
        .await
        .map_err(internal_error)?;
    Ok(Json(serde_json::json!({"ok": true})))
}

async fn get_metrics<L: LedgerAdapter + Clone>(
    State(runtime): State<SharedRuntime<L>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let metrics = runtime.metrics();
    Ok(Json(serde_json::json!({
        "shard_processed": metrics.shard_processed,
        "shard_rejected": metrics.shard_rejected,
        "risk_rejects": metrics.risk_rejects,
        "risk_limits": {
            "max_open_orders": runtime.risk_limits().max_open_orders,
            "max_qty_per_order": runtime.risk_limits().max_qty_per_order,
            "max_notional_per_order_czk": runtime.risk_limits().max_notional_per_order_czk,
            "max_short_exposure_czk": runtime.risk_limits().max_short_exposure_czk,
            "min_tick": runtime.risk_limits().min_tick,
            "max_tick": runtime.risk_limits().max_tick
        }
    })))
}

async fn get_routing<L: LedgerAdapter + Clone>(
    Path(market_id): Path<String>,
    State(runtime): State<SharedRuntime<L>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let shard = runtime.route_with_overrides(&market_id).await;
    Ok(Json(serde_json::json!({
        "market_id": market_id,
        "shard": shard,
        "shard_count": runtime.shard_count()
    })))
}

fn internal_error(err: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}
