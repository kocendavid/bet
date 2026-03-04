use std::sync::Arc;

use anyhow::Context;
use matcher_service::http::router;
use matcher_service::ledger::GrpcLedgerAdapter;
use matcher_service::persistence::Storage;
use matcher_service::service::MatcherService;
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let listen_addr = std::env::var("MATCHER_LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let ledger_addr = std::env::var("LEDGER_GRPC_ADDR").unwrap_or_else(|_| "http://127.0.0.1:50051".into());
    let market_id = std::env::var("MATCHER_MARKET_ID").unwrap_or_else(|_| "market-1".into());
    let outcome_id = std::env::var("MATCHER_OUTCOME_ID").unwrap_or_else(|_| "outcome-1".into());
    let data_dir = std::env::var("MATCHER_DATA_DIR").unwrap_or_else(|_| "./matcher-data".into());

    let storage = Storage::new(&data_dir);
    let ledger = GrpcLedgerAdapter::new(ledger_addr, 3, 50, 25);

    let matcher = MatcherService::new(market_id, outcome_id, storage, ledger, 100)
        .await
        .context("initialize matcher service")?;

    let app = router(Arc::new(Mutex::new(matcher)));
    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
