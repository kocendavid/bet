use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::admin::{
    AdminAuthorizer, AdminController, CreateMarketRequest, FileDisputeRequest,
    FinalizeResolutionRequest, ProposeResolutionRequest, SettleMarketRequest,
};
use crate::ledger::LedgerAdapter;
use crate::sharding::ShardRuntime;
use crate::streaming::{
    ChannelKind, StreamHub, StreamSubscription, SubscribeError, SubscriptionRequest, WsAuthorizer,
};
use crate::types::{CancelOrder, Command, CommandResult, PlaceOrder};

pub type SharedRuntime<L> = Arc<ShardRuntime<L>>;

#[derive(Clone)]
pub struct AppState<L: LedgerAdapter + Clone + 'static> {
    pub runtime: SharedRuntime<L>,
    pub admin: Arc<AdminController<L>>,
    pub admin_authorizer: AdminAuthorizer,
    pub stream_hub: Arc<StreamHub>,
    pub authorizer: Arc<dyn WsAuthorizer>,
    pub ws_queue_capacity: usize,
}

pub fn router<L: LedgerAdapter + Clone + 'static>(state: AppState<L>) -> Router {
    Router::new()
        .route("/orders", post(place_order::<L>))
        .route("/orders/{order_id}", delete(cancel_order::<L>))
        .route("/books/{market_id}/{outcome_id}", get(get_book::<L>))
        .route("/ws", get(ws_stream::<L>))
        .route("/admin/migrate", post(migrate_market::<L>))
        .route("/admin/metrics", get(get_metrics::<L>))
        .route("/admin/routing/{market_id}", get(get_routing::<L>))
        .route("/admin/markets", post(create_market::<L>))
        .route(
            "/admin/markets/{market_id}/resolution/propose",
            post(propose_resolution::<L>),
        )
        .route(
            "/admin/markets/{market_id}/disputes",
            post(file_dispute::<L>),
        )
        .route(
            "/admin/markets/{market_id}/resolution/finalize",
            post(finalize_resolution::<L>),
        )
        .route(
            "/admin/markets/{market_id}/settle",
            post(settle_market::<L>),
        )
        .with_state(state)
}

async fn place_order<L: LedgerAdapter + Clone>(
    State(state): State<AppState<L>>,
    Json(req): Json<PlaceOrder>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let cmd = Command::Place(req.clone());
    let out = state
        .runtime
        .process(cmd.clone())
        .await
        .map_err(internal_error)?;

    publish_stream_updates(&state, &cmd, &out).await;

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
    State(state): State<AppState<L>>,
    Json(body): Json<CancelBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let cmd = Command::Cancel(CancelOrder {
        command_id: body.command_id,
        market_id: body.market_id,
        outcome_id: body.outcome_id,
        user_id: body.user_id,
        order_id,
    });

    let out = state
        .runtime
        .process(cmd.clone())
        .await
        .map_err(internal_error)?;

    publish_stream_updates(&state, &cmd, &out).await;

    Ok(Json(serde_json::json!({
        "command_id": out.command_id,
        "events": out.events
    })))
}

async fn get_book<L: LedgerAdapter + Clone>(
    Path((market_id, outcome_id)): Path<(String, String)>,
    State(state): State<AppState<L>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    match state
        .runtime
        .get_book(market_id, outcome_id)
        .await
        .map_err(internal_error)?
    {
        Some(book) => Ok(Json(book)),
        None => Err((StatusCode::NOT_FOUND, "book not found".into())),
    }
}

#[derive(Debug, Deserialize)]
struct WsQuery {
    token: String,
    channel: String,
    market_id: Option<String>,
    user_id: Option<String>,
    include_depth_snapshots: Option<bool>,
}

async fn ws_stream<L: LedgerAdapter + Clone>(
    ws: WebSocketUpgrade,
    Query(query): Query<WsQuery>,
    State(state): State<AppState<L>>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let claims = state
        .authorizer
        .authorize(&query.token)
        .await
        .ok_or((StatusCode::UNAUTHORIZED, "unauthorized".to_string()))?;

    let channel = match query.channel.as_str() {
        "market" => ChannelKind::Market,
        "user" => ChannelKind::User,
        _ => return Err((StatusCode::BAD_REQUEST, "invalid channel".into())),
    };

    let subscription_request = SubscriptionRequest {
        channel,
        market_id: query.market_id,
        user_id: query.user_id,
        include_depth_snapshots: query.include_depth_snapshots.unwrap_or(false),
    };

    let subscription = state
        .stream_hub
        .subscribe(&claims, subscription_request, state.ws_queue_capacity)
        .await
        .map_err(|err| match err {
            SubscribeError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".to_string()),
        })?;

    let stream_hub = state.stream_hub.clone();
    Ok(ws.on_upgrade(move |socket| async move {
        run_ws_connection(socket, stream_hub, subscription).await;
    }))
}

#[derive(Debug, Deserialize)]
struct MigrateBody {
    market_id: String,
    outcome_id: String,
    target_shard: usize,
}

async fn migrate_market<L: LedgerAdapter + Clone>(
    State(state): State<AppState<L>>,
    Json(body): Json<MigrateBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    state
        .runtime
        .migrate_market(body.market_id, body.outcome_id, body.target_shard)
        .await
        .map_err(internal_error)?;
    Ok(Json(serde_json::json!({"ok": true})))
}

async fn get_metrics<L: LedgerAdapter + Clone>(
    State(state): State<AppState<L>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let metrics = state.runtime.metrics();
    let stream_metrics = state.stream_hub.metrics_snapshot().await;
    Ok(Json(serde_json::json!({
        "shard_processed": metrics.shard_processed,
        "shard_rejected": metrics.shard_rejected,
        "risk_rejects": metrics.risk_rejects,
        "streaming": stream_metrics,
        "risk_limits": {
            "max_open_orders": state.runtime.risk_limits().max_open_orders,
            "max_qty_per_order": state.runtime.risk_limits().max_qty_per_order,
            "max_notional_per_order_czk": state.runtime.risk_limits().max_notional_per_order_czk,
            "max_short_exposure_czk": state.runtime.risk_limits().max_short_exposure_czk,
            "min_tick": state.runtime.risk_limits().min_tick,
            "max_tick": state.runtime.risk_limits().max_tick
        }
    })))
}

async fn get_routing<L: LedgerAdapter + Clone>(
    Path(market_id): Path<String>,
    State(state): State<AppState<L>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let shard = state.runtime.route_with_overrides(&market_id).await;
    Ok(Json(serde_json::json!({
        "market_id": market_id,
        "shard": shard,
        "shard_count": state.runtime.shard_count()
    })))
}

async fn create_market<L: LedgerAdapter + Clone>(
    State(state): State<AppState<L>>,
    headers: HeaderMap,
    Json(body): Json<CreateMarketRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = require_admin_actor(&state, &headers)?;
    let market = state
        .admin
        .create_market(&actor, body)
        .await
        .map_err(bad_request)?;
    Ok(Json(serde_json::json!({ "market": market })))
}

async fn propose_resolution<L: LedgerAdapter + Clone>(
    Path(market_id): Path<String>,
    State(state): State<AppState<L>>,
    headers: HeaderMap,
    Json(body): Json<ProposeResolutionRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = require_admin_actor(&state, &headers)?;
    let market = state
        .admin
        .propose_resolution(&actor, &market_id, body)
        .await
        .map_err(bad_request)?;
    Ok(Json(serde_json::json!({ "market": market })))
}

async fn file_dispute<L: LedgerAdapter + Clone>(
    Path(market_id): Path<String>,
    State(state): State<AppState<L>>,
    headers: HeaderMap,
    Json(body): Json<FileDisputeRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = require_admin_actor(&state, &headers)?;
    let market = state
        .admin
        .file_dispute(&actor, &market_id, body)
        .await
        .map_err(bad_request)?;
    Ok(Json(serde_json::json!({ "market": market })))
}

async fn finalize_resolution<L: LedgerAdapter + Clone>(
    Path(market_id): Path<String>,
    State(state): State<AppState<L>>,
    headers: HeaderMap,
    Json(body): Json<FinalizeResolutionRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = require_admin_actor(&state, &headers)?;
    let market = state
        .admin
        .finalize_resolution(&actor, &market_id, body)
        .await
        .map_err(bad_request)?;
    Ok(Json(serde_json::json!({ "market": market })))
}

async fn settle_market<L: LedgerAdapter + Clone>(
    Path(market_id): Path<String>,
    State(state): State<AppState<L>>,
    headers: HeaderMap,
    Json(body): Json<SettleMarketRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = require_admin_actor(&state, &headers)?;
    let (market, outcome) = state
        .admin
        .settle_market(&actor, &market_id, body)
        .await
        .map_err(bad_request)?;
    Ok(Json(serde_json::json!({
        "market": market,
        "idempotent": outcome.idempotent
    })))
}

fn internal_error(err: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

fn bad_request(err: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, err.to_string())
}

fn require_admin_actor<L: LedgerAdapter + Clone>(
    state: &AppState<L>,
    headers: &HeaderMap,
) -> Result<String, (StatusCode, String)> {
    let Some(raw_token) = headers
        .get("x-admin-token")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
    else {
        return Err((StatusCode::UNAUTHORIZED, "missing admin token".into()));
    };
    let Some(role) = state.admin_authorizer.role_for_token(raw_token) else {
        return Err((StatusCode::UNAUTHORIZED, "invalid admin token".into()));
    };
    if role != "admin" {
        return Err((StatusCode::FORBIDDEN, "insufficient admin role".into()));
    }
    Ok(format!("admin:{raw_token}"))
}

async fn publish_stream_updates<L: LedgerAdapter + Clone>(
    state: &AppState<L>,
    command: &Command,
    out: &CommandResult,
) {
    state.stream_hub.publish_command_result(command, out).await;

    let (market_id, outcome_id) = match command {
        Command::Place(place) => (&place.market_id, &place.outcome_id),
        Command::Cancel(cancel) => (&cancel.market_id, &cancel.outcome_id),
    };

    let maybe_book = state
        .runtime
        .get_book(market_id.clone(), outcome_id.clone())
        .await
        .ok()
        .flatten();

    if let Some(book) = maybe_book {
        if let (Some(bids), Some(asks)) = (
            parse_levels(book.get("bids")),
            parse_levels(book.get("asks")),
        ) {
            state
                .stream_hub
                .publish_book_snapshot(market_id.clone(), outcome_id.clone(), bids, asks)
                .await;
        }
    }
}

fn parse_levels(levels: Option<&serde_json::Value>) -> Option<Vec<(i64, i64)>> {
    let arr = levels?.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let pair = item.as_array()?;
        if pair.len() != 2 {
            return None;
        }
        let price = pair.first()?.as_i64()?;
        let qty = pair.get(1)?.as_i64()?;
        out.push((price, qty));
    }
    Some(out)
}

async fn run_ws_connection(
    socket: WebSocket,
    stream_hub: Arc<StreamHub>,
    subscription: StreamSubscription,
) {
    let connection_id = subscription.connection_id;
    let mut receiver = subscription.receiver;
    let mut socket = socket;

    while let Some(envelope) = receiver.recv().await {
        let payload = match serde_json::to_string(&envelope) {
            Ok(payload) => payload,
            Err(_) => continue,
        };

        if socket.send(Message::Text(payload.into())).await.is_err() {
            break;
        }
    }

    stream_hub.disconnect(connection_id).await;
}
