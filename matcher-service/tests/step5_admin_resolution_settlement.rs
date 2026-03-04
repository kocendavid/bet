use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use matcher_service::admin::{AdminAuthorizer, AdminController};
use matcher_service::http::{router, AppState};
use matcher_service::ledger::{FillIntent, LedgerAdapter, LedgerError, ReservationKindLocal};
use matcher_service::order_index::OrderIndex;
use matcher_service::risk::RiskLimits;
use matcher_service::sharding::ShardRuntime;
use matcher_service::streaming::{StaticTokenAuthorizer, StreamHub, WsAuthorizer};
use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::sync::Mutex;
use tower::ServiceExt;

#[derive(Default, Clone)]
struct MockLedger {
    settle_calls: Arc<Mutex<Vec<(String, String, String)>>>,
}

#[tonic::async_trait]
impl LedgerAdapter for MockLedger {
    async fn reserve_for_order(
        &self,
        _command_id: &str,
        _market_id: &str,
        _outcome_id: &str,
        _user_id: &str,
        _order_id: &str,
        _kind: ReservationKindLocal,
        _amount_czk: i64,
    ) -> Result<(), LedgerError> {
        Ok(())
    }

    async fn release_reservation(
        &self,
        _command_id: &str,
        _order_id: &str,
    ) -> Result<(), LedgerError> {
        Ok(())
    }

    async fn adjust_reservation(
        &self,
        _command_id: &str,
        _order_id: &str,
        _delta_czk: i64,
    ) -> Result<(), LedgerError> {
        Ok(())
    }

    async fn apply_fill(&self, _intent: FillIntent) -> Result<(), LedgerError> {
        Ok(())
    }

    async fn settle_market(
        &self,
        command_id: &str,
        idempotency_key: &str,
        market_id: &str,
        _winning_outcome_id: &str,
        _chunk_size: u32,
    ) -> Result<(), LedgerError> {
        self.settle_calls.lock().await.push((
            command_id.to_string(),
            idempotency_key.to_string(),
            market_id.to_string(),
        ));
        Ok(())
    }
}

async fn test_app(
    clock: Arc<AtomicI64>,
) -> (axum::Router, Arc<Mutex<Vec<(String, String, String)>>>) {
    std::env::set_var("ADMIN_API_TOKENS", "admin-token:admin");
    let data_path = tempdir().unwrap().keep();
    let ledger = MockLedger::default();
    let settle_calls = ledger.settle_calls.clone();
    let runtime =
        ShardRuntime::new(2, &data_path, ledger.clone(), 0, RiskLimits::default()).unwrap();
    let admin = AdminController::new(&data_path, ledger)
        .await
        .unwrap()
        .with_clock({
            let clock = clock.clone();
            Arc::new(move || clock.load(Ordering::SeqCst))
        });

    let app = router(AppState {
        runtime: Arc::new(runtime),
        order_index: Arc::new(
            OrderIndex::load(
                std::path::Path::new(&data_path).join("admin/order-index.json"),
                48 * 3600,
            )
            .await
            .unwrap(),
        ),
        admin: Arc::new(admin),
        admin_authorizer: AdminAuthorizer::from_env(),
        stream_hub: Arc::new(StreamHub::new()),
        authorizer: Arc::new(StaticTokenAuthorizer::from_env()) as Arc<dyn WsAuthorizer>,
        ws_queue_capacity: 8,
    });
    (app, settle_calls)
}

async fn post_json(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("content-type", "application/json")
                .header("x-admin-token", "admin-token")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let parsed = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
        json!({
            "raw": String::from_utf8_lossy(&bytes).to_string()
        })
    });
    (status, parsed)
}

#[tokio::test]
async fn lifecycle_integration_create_resolve_settle_and_idempotency() {
    let clock = Arc::new(AtomicI64::new(1_000));
    let (app, settle_calls) = test_app(clock.clone()).await;

    let (status, create) = post_json(
        &app,
        "/admin/markets",
        json!({
            "market_id": "m-step5",
            "outcomes": ["yes", "no"],
            "sources": ["oracle-a"],
            "criteria": "event happened",
            "dispute_window_secs": 10,
            "price_bands": {"min_tick": 1000, "max_tick": 9000},
            "fee_config": {"taker_fee_bps": 25}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(create["market"]["state"], "open");

    let (_status, _proposed) = post_json(
        &app,
        "/admin/markets/m-step5/resolution/propose",
        json!({"outcome_id":"yes"}),
    )
    .await;

    clock.store(11_100, Ordering::SeqCst);
    let (finalize_status, finalized) = post_json(
        &app,
        "/admin/markets/m-step5/resolution/finalize",
        json!({"outcome_id":"yes"}),
    )
    .await;
    assert_eq!(finalize_status, StatusCode::OK);
    assert_eq!(finalized["market"]["state"], "finalized");

    let (settle_status, settled) = post_json(
        &app,
        "/admin/markets/m-step5/settle",
        json!({"idempotency_key":"set-1","chunk_size":32}),
    )
    .await;
    assert_eq!(settle_status, StatusCode::OK);
    assert_eq!(settled["market"]["state"], "settled");
    assert_eq!(settled["idempotent"], false);

    let (repeat_status, repeat) = post_json(
        &app,
        "/admin/markets/m-step5/settle",
        json!({"idempotency_key":"set-1","chunk_size":32}),
    )
    .await;
    assert_eq!(repeat_status, StatusCode::OK);
    assert_eq!(repeat["idempotent"], true);

    let (dup_status, _dup) = post_json(
        &app,
        "/admin/markets/m-step5/settle",
        json!({"idempotency_key":"set-2","chunk_size":32}),
    )
    .await;
    assert_eq!(dup_status, StatusCode::BAD_REQUEST);

    let calls = settle_calls.lock().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, "set-1");
}

#[tokio::test]
async fn dispute_window_boundaries_enforced() {
    let clock = Arc::new(AtomicI64::new(5_000));
    let (app, _) = test_app(clock.clone()).await;

    let _ = post_json(
        &app,
        "/admin/markets",
        json!({
            "market_id": "m-boundary",
            "outcomes": ["yes", "no"],
            "sources": ["oracle-a"],
            "criteria": "event happened",
            "dispute_window_secs": 10,
            "price_bands": {"min_tick": 1000, "max_tick": 9000},
            "fee_config": {"taker_fee_bps": 25}
        }),
    )
    .await;
    let _ = post_json(
        &app,
        "/admin/markets/m-boundary/resolution/propose",
        json!({"outcome_id":"yes"}),
    )
    .await;

    clock.store(5_100, Ordering::SeqCst);
    let (early_finalize_status, _early_finalize) = post_json(
        &app,
        "/admin/markets/m-boundary/resolution/finalize",
        json!({"outcome_id":"yes"}),
    )
    .await;
    assert_eq!(early_finalize_status, StatusCode::BAD_REQUEST);

    clock.store(15_000, Ordering::SeqCst);
    let (dispute_status, dispute) = post_json(
        &app,
        "/admin/markets/m-boundary/disputes",
        json!({"user_id":"reviewer","bond_czk":1000}),
    )
    .await;
    assert_eq!(dispute_status, StatusCode::OK);
    assert_eq!(dispute["market"]["state"], "in_review");

    let (finalize_status, finalized) = post_json(
        &app,
        "/admin/markets/m-boundary/resolution/finalize",
        json!({"outcome_id":"no"}),
    )
    .await;
    assert_eq!(finalize_status, StatusCode::OK);
    assert_eq!(finalized["market"]["state"], "finalized");
}
