use anyhow::Context;
use matcher_service::http::router;
use matcher_service::ledger::GrpcLedgerAdapter;
use matcher_service::risk::RiskLimits;
use matcher_service::sharding::ShardRuntime;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let listen_addr =
        std::env::var("MATCHER_LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let ledger_addr =
        std::env::var("LEDGER_GRPC_ADDR").unwrap_or_else(|_| "http://127.0.0.1:50051".into());
    let shard_count: usize = std::env::var("MATCHER_SHARD_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    let data_dir = std::env::var("MATCHER_DATA_DIR").unwrap_or_else(|_| "./matcher-data".into());

    let ledger = GrpcLedgerAdapter::new(ledger_addr, 3, 50, 25);
    let risk_limits = RiskLimits {
        max_open_orders: std::env::var("RISK_MAX_OPEN_ORDERS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100),
        max_qty_per_order: std::env::var("RISK_MAX_QTY_PER_ORDER")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1_000_000),
        max_notional_per_order_czk: std::env::var("RISK_MAX_NOTIONAL_PER_ORDER_CZK")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10_000_000_000),
        max_short_exposure_czk: std::env::var("RISK_MAX_SHORT_EXPOSURE_CZK")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10_000_000_000),
        min_tick: std::env::var("RISK_MIN_TICK")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        max_tick: std::env::var("RISK_MAX_TICK")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(matcher_service::types::FACE_CZK),
    };

    let runtime = ShardRuntime::new(shard_count, data_dir, ledger, 100, risk_limits)
        .context("initialize matcher service")?;

    let app = router(std::sync::Arc::new(runtime));
    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
