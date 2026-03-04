use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::ledger::LedgerAdapter;
use crate::service::MatcherService;
use crate::types::{CancelOrder, Command, PlaceOrder};

pub type SharedService<L> = Arc<Mutex<MatcherService<L>>>;

pub fn router<L: LedgerAdapter + 'static>(service: SharedService<L>) -> Router {
    Router::new()
        .route("/orders", post(place_order::<L>))
        .route("/orders/:order_id", delete(cancel_order::<L>))
        .route("/books/:market_id/:outcome_id", get(get_book::<L>))
        .with_state(service)
}

async fn place_order<L: LedgerAdapter>(
    State(service): State<SharedService<L>>,
    Json(req): Json<PlaceOrder>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mut svc = service.lock().await;
    let out = svc
        .process(Command::Place(req))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({
        "command_id": out.command_id,
        "events": out.events
    })))
}

#[derive(Debug, Deserialize)]
struct CancelBody {
    command_id: String,
    user_id: String,
}

async fn cancel_order<L: LedgerAdapter>(
    Path(order_id): Path<String>,
    State(service): State<SharedService<L>>,
    Json(body): Json<CancelBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let cmd = CancelOrder {
        command_id: body.command_id,
        user_id: body.user_id,
        order_id,
    };

    let mut svc = service.lock().await;
    let out = svc
        .process(Command::Cancel(cmd))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({
        "command_id": out.command_id,
        "events": out.events
    })))
}

async fn get_book<L: LedgerAdapter>(
    Path((market_id, outcome_id)): Path<(String, String)>,
    State(service): State<SharedService<L>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let svc = service.lock().await;
    if svc.book.market_id != market_id || svc.book.outcome_id != outcome_id {
        return Err((StatusCode::NOT_FOUND, "book not found".into()));
    }
    Ok(Json(serde_json::json!(svc.book.view())))
}
