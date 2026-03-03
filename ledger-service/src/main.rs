use anyhow::Context;
use ledger_service::pb::ledger_service_server::LedgerServiceServer;
use ledger_service::service::LedgerGrpcService;
use sqlx::postgres::PgPoolOptions;
use tonic::transport::Server;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL is required")?;
    let listen_addr =
        std::env::var("LEDGER_LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:50051".into());

    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect(&database_url)
        .await
        .context("db connect failed")?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("migrations failed")?;

    let service = LedgerGrpcService::new(pool);
    Server::builder()
        .add_service(LedgerServiceServer::new(service))
        .serve(listen_addr.parse()?)
        .await?;

    Ok(())
}
