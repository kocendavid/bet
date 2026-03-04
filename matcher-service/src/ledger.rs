use std::time::Duration;

use crate::pb::ledger_service_client::LedgerServiceClient;
use crate::pb::{
    AdjustReservationRequest, ApplyFillRequest, Party, ReleaseReservationRequest, ReservationKind,
    ReserveForOrderRequest, SettleMarketRequest, Side as LedgerSide,
};
use thiserror::Error;
use tonic::transport::Channel;
use tonic::{Code, Status};

use crate::types::Side;

#[derive(Debug, Clone)]
pub enum ReservationKindLocal {
    Buy,
    ShortCollateral,
}

#[derive(Debug, Clone)]
pub struct FillIntent {
    pub fill_id: String,
    pub maker_user_id: String,
    pub maker_order_id: String,
    pub taker_user_id: String,
    pub taker_order_id: String,
    pub market_id: String,
    pub outcome_id: String,
    pub qty: i64,
    pub price_czk: i64,
    pub taker_side: Side,
    pub fee_bps: i64,
}

#[derive(Debug, Error)]
pub enum LedgerError {
    #[error("ledger rejected: {0}")]
    Rejected(String),
    #[error("ledger transient failure after retries: {0}")]
    Transient(String),
    #[error("ledger rpc failed: {0}")]
    Rpc(String),
}

#[tonic::async_trait]
pub trait LedgerAdapter: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn reserve_for_order(
        &self,
        command_id: &str,
        market_id: &str,
        outcome_id: &str,
        user_id: &str,
        order_id: &str,
        kind: ReservationKindLocal,
        amount_czk: i64,
    ) -> Result<(), LedgerError>;

    async fn release_reservation(
        &self,
        command_id: &str,
        order_id: &str,
    ) -> Result<(), LedgerError>;

    async fn adjust_reservation(
        &self,
        command_id: &str,
        order_id: &str,
        delta_czk: i64,
    ) -> Result<(), LedgerError>;

    async fn apply_fill(&self, intent: FillIntent) -> Result<(), LedgerError>;

    async fn settle_market(
        &self,
        command_id: &str,
        idempotency_key: &str,
        market_id: &str,
        winning_outcome_id: &str,
        chunk_size: u32,
    ) -> Result<(), LedgerError>;
}

#[derive(Clone)]
pub struct GrpcLedgerAdapter {
    endpoint: String,
    max_retries: usize,
    retry_backoff_ms: u64,
    fee_bps: i64,
}

#[derive(Debug, Clone, Default)]
pub struct AcceptAllLedgerAdapter;

#[derive(Clone)]
pub enum ConfiguredLedgerAdapter {
    Grpc(GrpcLedgerAdapter),
    AcceptAll(AcceptAllLedgerAdapter),
}

impl GrpcLedgerAdapter {
    pub fn new(endpoint: String, max_retries: usize, retry_backoff_ms: u64, fee_bps: i64) -> Self {
        Self {
            endpoint,
            max_retries,
            retry_backoff_ms,
            fee_bps,
        }
    }

    async fn client(&self) -> Result<LedgerServiceClient<Channel>, LedgerError> {
        LedgerServiceClient::connect(self.endpoint.clone())
            .await
            .map_err(|e| LedgerError::Rpc(format!("connect failed: {e}")))
    }

    async fn with_retry<T, F, Fut>(&self, mut f: F) -> Result<T, LedgerError>
    where
        F: FnMut() -> Fut + Send,
        Fut: std::future::Future<Output = Result<T, Status>> + Send,
        T: Send,
    {
        let mut attempt = 0usize;
        loop {
            match f().await {
                Ok(v) => return Ok(v),
                Err(status) if is_transient(&status) && attempt < self.max_retries => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(self.retry_backoff_ms)).await;
                }
                Err(status) if is_transient(&status) => {
                    return Err(LedgerError::Transient(status.to_string()));
                }
                Err(status) => return Err(LedgerError::Rpc(status.to_string())),
            }
        }
    }
}

fn is_transient(status: &Status) -> bool {
    matches!(
        status.code(),
        Code::Unavailable | Code::DeadlineExceeded | Code::Cancelled | Code::Unknown
    )
}

#[tonic::async_trait]
impl LedgerAdapter for AcceptAllLedgerAdapter {
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
        _command_id: &str,
        _idempotency_key: &str,
        _market_id: &str,
        _winning_outcome_id: &str,
        _chunk_size: u32,
    ) -> Result<(), LedgerError> {
        Ok(())
    }
}

#[tonic::async_trait]
impl LedgerAdapter for ConfiguredLedgerAdapter {
    async fn reserve_for_order(
        &self,
        command_id: &str,
        market_id: &str,
        outcome_id: &str,
        user_id: &str,
        order_id: &str,
        kind: ReservationKindLocal,
        amount_czk: i64,
    ) -> Result<(), LedgerError> {
        match self {
            Self::Grpc(inner) => {
                inner
                    .reserve_for_order(
                        command_id, market_id, outcome_id, user_id, order_id, kind, amount_czk,
                    )
                    .await
            }
            Self::AcceptAll(inner) => {
                inner
                    .reserve_for_order(
                        command_id, market_id, outcome_id, user_id, order_id, kind, amount_czk,
                    )
                    .await
            }
        }
    }

    async fn release_reservation(
        &self,
        command_id: &str,
        order_id: &str,
    ) -> Result<(), LedgerError> {
        match self {
            Self::Grpc(inner) => inner.release_reservation(command_id, order_id).await,
            Self::AcceptAll(inner) => inner.release_reservation(command_id, order_id).await,
        }
    }

    async fn adjust_reservation(
        &self,
        command_id: &str,
        order_id: &str,
        delta_czk: i64,
    ) -> Result<(), LedgerError> {
        match self {
            Self::Grpc(inner) => {
                inner
                    .adjust_reservation(command_id, order_id, delta_czk)
                    .await
            }
            Self::AcceptAll(inner) => {
                inner
                    .adjust_reservation(command_id, order_id, delta_czk)
                    .await
            }
        }
    }

    async fn apply_fill(&self, intent: FillIntent) -> Result<(), LedgerError> {
        match self {
            Self::Grpc(inner) => inner.apply_fill(intent).await,
            Self::AcceptAll(inner) => inner.apply_fill(intent).await,
        }
    }

    async fn settle_market(
        &self,
        command_id: &str,
        idempotency_key: &str,
        market_id: &str,
        winning_outcome_id: &str,
        chunk_size: u32,
    ) -> Result<(), LedgerError> {
        match self {
            Self::Grpc(inner) => {
                inner
                    .settle_market(
                        command_id,
                        idempotency_key,
                        market_id,
                        winning_outcome_id,
                        chunk_size,
                    )
                    .await
            }
            Self::AcceptAll(inner) => {
                inner
                    .settle_market(
                        command_id,
                        idempotency_key,
                        market_id,
                        winning_outcome_id,
                        chunk_size,
                    )
                    .await
            }
        }
    }
}

#[tonic::async_trait]
impl LedgerAdapter for GrpcLedgerAdapter {
    async fn reserve_for_order(
        &self,
        command_id: &str,
        market_id: &str,
        outcome_id: &str,
        user_id: &str,
        order_id: &str,
        kind: ReservationKindLocal,
        amount_czk: i64,
    ) -> Result<(), LedgerError> {
        let client = self.client().await?;
        let kind_pb = match kind {
            ReservationKindLocal::Buy => ReservationKind::BuyReserve,
            ReservationKindLocal::ShortCollateral => ReservationKind::ShortCollateralReserve,
        };
        let req = ReserveForOrderRequest {
            command_id: command_id.to_string(),
            reservation_id: order_id.to_string(),
            user_id: user_id.to_string(),
            order_id: order_id.to_string(),
            kind: kind_pb as i32,
            amount_czk,
            market_id: market_id.to_string(),
            outcome_id: outcome_id.to_string(),
        };

        let resp = self
            .with_retry(|| {
                let mut c = client.clone();
                let r = req.clone();
                async move { c.reserve_for_order(r).await.map(|r| r.into_inner()) }
            })
            .await?;

        if !resp.ok {
            return Err(LedgerError::Rejected(resp.reason));
        }
        Ok(())
    }

    async fn release_reservation(
        &self,
        command_id: &str,
        order_id: &str,
    ) -> Result<(), LedgerError> {
        let client = self.client().await?;
        let req = ReleaseReservationRequest {
            command_id: command_id.to_string(),
            reservation_id: order_id.to_string(),
        };

        let resp = self
            .with_retry(|| {
                let mut c = client.clone();
                let r = req.clone();
                async move { c.release_reservation(r).await.map(|r| r.into_inner()) }
            })
            .await?;
        if !resp.ok {
            return Err(LedgerError::Rejected(resp.reason));
        }
        Ok(())
    }

    async fn adjust_reservation(
        &self,
        command_id: &str,
        order_id: &str,
        delta_czk: i64,
    ) -> Result<(), LedgerError> {
        let client = self.client().await?;
        let req = AdjustReservationRequest {
            command_id: command_id.to_string(),
            reservation_id: order_id.to_string(),
            delta_czk,
        };

        let resp = self
            .with_retry(|| {
                let mut c = client.clone();
                let r = req.clone();
                async move { c.adjust_reservation(r).await.map(|r| r.into_inner()) }
            })
            .await?;
        if !resp.ok {
            return Err(LedgerError::Rejected(resp.reason));
        }
        Ok(())
    }

    async fn apply_fill(&self, intent: FillIntent) -> Result<(), LedgerError> {
        let client = self.client().await?;
        let taker_side = match intent.taker_side {
            Side::Buy => LedgerSide::Buy,
            Side::Sell => LedgerSide::Sell,
        };

        let req = ApplyFillRequest {
            fill_id: intent.fill_id,
            maker: Some(Party {
                user_id: intent.maker_user_id,
                reservation_id: intent.maker_order_id.clone(),
                order_id: intent.maker_order_id,
            }),
            taker: Some(Party {
                user_id: intent.taker_user_id,
                reservation_id: intent.taker_order_id.clone(),
                order_id: intent.taker_order_id,
            }),
            market_id: intent.market_id,
            outcome_id: intent.outcome_id,
            qty: intent.qty,
            price_czk: intent.price_czk,
            taker_side: taker_side as i32,
            fee_bps: self.fee_bps,
        };

        let resp = self
            .with_retry(|| {
                let mut c = client.clone();
                let r = req.clone();
                async move { c.apply_fill(r).await.map(|r| r.into_inner()) }
            })
            .await?;
        if !resp.ok {
            return Err(LedgerError::Rejected(resp.reason));
        }
        Ok(())
    }

    async fn settle_market(
        &self,
        command_id: &str,
        idempotency_key: &str,
        market_id: &str,
        winning_outcome_id: &str,
        chunk_size: u32,
    ) -> Result<(), LedgerError> {
        let client = self.client().await?;
        let req = SettleMarketRequest {
            command_id: command_id.to_string(),
            idempotency_key: idempotency_key.to_string(),
            market_id: market_id.to_string(),
            winning_outcome_id: winning_outcome_id.to_string(),
            chunk_size,
        };

        let resp = self
            .with_retry(|| {
                let mut c = client.clone();
                let r = req.clone();
                async move { c.settle_market(r).await.map(|r| r.into_inner()) }
            })
            .await?;
        if !resp.ok {
            return Err(LedgerError::Rejected(resp.reason));
        }
        Ok(())
    }
}
