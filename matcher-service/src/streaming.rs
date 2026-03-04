use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

use crate::types::{Command, CommandResult, Event};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    Market,
    User,
}

#[derive(Debug, Clone)]
pub struct AuthClaims {
    pub user_id: Option<String>,
    pub can_market_stream: bool,
}

#[tonic::async_trait]
pub trait WsAuthorizer: Send + Sync {
    async fn authorize(&self, token: &str) -> Option<AuthClaims>;
}

#[derive(Default)]
pub struct StaticTokenAuthorizer {
    claims_by_token: HashMap<String, AuthClaims>,
}

impl StaticTokenAuthorizer {
    pub fn from_env() -> Self {
        // token:user_id entries, where user_id "*" grants market stream-only access.
        let raw = std::env::var("MATCHER_WS_TOKENS").unwrap_or_default();
        let mut claims_by_token = HashMap::new();
        for entry in raw.split(',') {
            let trimmed = entry.trim();
            if trimmed.is_empty() {
                continue;
            }
            let mut parts = trimmed.splitn(2, ':');
            let token = parts.next().unwrap_or_default().trim();
            let user = parts.next().unwrap_or_default().trim();
            if token.is_empty() || user.is_empty() {
                continue;
            }

            let claims = if user == "*" {
                AuthClaims {
                    user_id: None,
                    can_market_stream: true,
                }
            } else {
                AuthClaims {
                    user_id: Some(user.to_string()),
                    can_market_stream: true,
                }
            };
            claims_by_token.insert(token.to_string(), claims);
        }
        Self { claims_by_token }
    }

    pub fn with_tokens(tokens: impl IntoIterator<Item = (String, AuthClaims)>) -> Self {
        Self {
            claims_by_token: tokens.into_iter().collect(),
        }
    }
}

#[tonic::async_trait]
impl WsAuthorizer for StaticTokenAuthorizer {
    async fn authorize(&self, token: &str) -> Option<AuthClaims> {
        self.claims_by_token.get(token).cloned()
    }
}

#[derive(Debug, Clone)]
pub struct SubscriptionRequest {
    pub channel: ChannelKind,
    pub market_id: Option<String>,
    pub user_id: Option<String>,
    pub include_depth_snapshots: bool,
}

#[derive(Debug)]
pub enum SubscribeError {
    Unauthorized,
}

#[derive(Debug)]
pub struct StreamSubscription {
    pub connection_id: u64,
    pub receiver: mpsc::Receiver<StreamEnvelope>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamPayload {
    MarketTrade {
        market_id: String,
        outcome_id: String,
        fill_id: String,
        maker_order_id: String,
        taker_order_id: String,
        price_czk: i64,
        qty: i64,
    },
    MarketTopOfBook {
        market_id: String,
        outcome_id: String,
        best_bid_czk: i64,
        best_ask_czk: i64,
    },
    MarketDepthSnapshot {
        market_id: String,
        outcome_id: String,
        bids: Vec<(i64, i64)>,
        asks: Vec<(i64, i64)>,
    },
    UserOrderUpdate {
        user_id: String,
        market_id: String,
        outcome_id: String,
        order_id: String,
        state: String,
        reason: Option<String>,
        qty_remaining: Option<i64>,
    },
    UserFill {
        user_id: String,
        market_id: String,
        outcome_id: String,
        fill_id: String,
        order_id: String,
        price_czk: i64,
        qty: i64,
    },
    UserBalanceUpdate {
        user_id: String,
        available_czk: i64,
        reserved_czk: i64,
    },
    AuditEvent {
        source: String,
        payload_json: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamEnvelope {
    pub version: u32,
    pub event_seq: u64,
    pub event_id: String,
    pub source: String,
    pub partition_key: String,
    pub emitted_at_ms: i64,
    pub critical: bool,
    pub payload: StreamPayload,
}

impl StreamEnvelope {
    pub fn is_depth_snapshot(&self) -> bool {
        matches!(self.payload, StreamPayload::MarketDepthSnapshot { .. })
    }
}

#[derive(Default)]
pub struct StreamMetrics {
    unauthorized: std::sync::atomic::AtomicU64,
    delivered: std::sync::atomic::AtomicU64,
    depth_dropped: std::sync::atomic::AtomicU64,
    slow_consumer_disconnects: std::sync::atomic::AtomicU64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamMetricsSnapshot {
    pub active_connections: usize,
    pub unauthorized: u64,
    pub delivered: u64,
    pub depth_dropped: u64,
    pub slow_consumer_disconnects: u64,
}

impl StreamMetrics {
    fn snapshot(&self, active_connections: usize) -> StreamMetricsSnapshot {
        StreamMetricsSnapshot {
            active_connections,
            unauthorized: self.unauthorized.load(std::sync::atomic::Ordering::Relaxed),
            delivered: self.delivered.load(std::sync::atomic::Ordering::Relaxed),
            depth_dropped: self
                .depth_dropped
                .load(std::sync::atomic::Ordering::Relaxed),
            slow_consumer_disconnects: self
                .slow_consumer_disconnects
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }
}

#[derive(Clone)]
struct Subscriber {
    request: SubscriptionRequest,
    tx: mpsc::Sender<StreamEnvelope>,
}

struct HubState {
    next_conn_id: u64,
    subscribers: HashMap<u64, Subscriber>,
    event_seq_by_key: HashMap<String, u64>,
    projection_seq_by_key: HashMap<String, u64>,
}

impl HubState {
    fn new() -> Self {
        Self {
            next_conn_id: 1,
            subscribers: HashMap::new(),
            event_seq_by_key: HashMap::new(),
            projection_seq_by_key: HashMap::new(),
        }
    }

    fn next_event_seq(&mut self, partition_key: &str) -> u64 {
        let entry = self
            .event_seq_by_key
            .entry(partition_key.to_string())
            .or_insert(0);
        *entry += 1;
        *entry
    }
}

pub struct StreamHub {
    state: Arc<Mutex<HubState>>,
    metrics: Arc<StreamMetrics>,
}

impl Default for StreamHub {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamHub {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(HubState::new())),
            metrics: Arc::new(StreamMetrics::default()),
        }
    }

    pub async fn metrics_snapshot(&self) -> StreamMetricsSnapshot {
        let active_connections = self.state.lock().await.subscribers.len();
        self.metrics.snapshot(active_connections)
    }

    pub async fn subscribe(
        &self,
        claims: &AuthClaims,
        request: SubscriptionRequest,
        queue_capacity: usize,
    ) -> Result<StreamSubscription, SubscribeError> {
        if !self.authorized(claims, &request) {
            self.metrics
                .unauthorized
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Err(SubscribeError::Unauthorized);
        }

        let capacity = queue_capacity.max(1);
        let (tx, receiver) = mpsc::channel(capacity);
        let mut state = self.state.lock().await;
        let connection_id = state.next_conn_id;
        state.next_conn_id += 1;
        state
            .subscribers
            .insert(connection_id, Subscriber { request, tx });

        Ok(StreamSubscription {
            connection_id,
            receiver,
        })
    }

    pub async fn disconnect(&self, connection_id: u64) {
        self.state.lock().await.subscribers.remove(&connection_id);
    }

    pub async fn publish_command_result(&self, command: &Command, result: &CommandResult) {
        let (user_id, market_id, outcome_id) = match command {
            Command::Place(place) => (
                place.user_id.clone(),
                place.market_id.clone(),
                place.outcome_id.clone(),
            ),
            Command::Cancel(cancel) => (
                cancel.user_id.clone(),
                cancel.market_id.clone(),
                cancel.outcome_id.clone(),
            ),
        };

        for event in &result.events {
            let order_payload = user_order_payload(&user_id, &market_id, &outcome_id, event);
            self.publish(
                format!("user:{user_id}"),
                true,
                order_payload,
                "matcher".to_string(),
            )
            .await;

            if let Event::TradeExecuted {
                fill_id,
                taker_order_id,
                price,
                qty,
                maker_order_id,
            } = event
            {
                self.publish(
                    market_id.clone(),
                    true,
                    StreamPayload::MarketTrade {
                        market_id: market_id.clone(),
                        outcome_id: outcome_id.clone(),
                        fill_id: fill_id.clone(),
                        maker_order_id: maker_order_id.clone(),
                        taker_order_id: taker_order_id.clone(),
                        price_czk: *price,
                        qty: *qty,
                    },
                    "matcher".to_string(),
                )
                .await;

                self.publish(
                    format!("user:{user_id}"),
                    true,
                    StreamPayload::UserFill {
                        user_id: user_id.clone(),
                        market_id: market_id.clone(),
                        outcome_id: outcome_id.clone(),
                        fill_id: fill_id.clone(),
                        order_id: taker_order_id.clone(),
                        price_czk: *price,
                        qty: *qty,
                    },
                    "matcher".to_string(),
                )
                .await;
            }
        }
    }

    pub async fn publish_book_snapshot(
        &self,
        market_id: String,
        outcome_id: String,
        bids: Vec<(i64, i64)>,
        asks: Vec<(i64, i64)>,
    ) {
        let best_bid_czk = bids.first().map(|(p, _)| *p).unwrap_or(0);
        let best_ask_czk = asks.first().map(|(p, _)| *p).unwrap_or(0);

        self.publish(
            market_id.clone(),
            true,
            StreamPayload::MarketTopOfBook {
                market_id: market_id.clone(),
                outcome_id: outcome_id.clone(),
                best_bid_czk,
                best_ask_czk,
            },
            "matcher".to_string(),
        )
        .await;

        self.publish(
            market_id.clone(),
            false,
            StreamPayload::MarketDepthSnapshot {
                market_id,
                outcome_id: outcome_id.clone(),
                bids,
                asks,
            },
            "matcher".to_string(),
        )
        .await;
    }

    pub async fn ingest_market_projection(
        &self,
        source: &str,
        market_id: &str,
        projection_seq: u64,
        payload: StreamPayload,
    ) {
        let key = format!("{source}:{market_id}");
        {
            let mut state = self.state.lock().await;
            let last = state.projection_seq_by_key.entry(key).or_insert(0);
            if projection_seq <= *last {
                return;
            }
            *last = projection_seq;
        }

        self.publish(market_id.to_string(), false, payload, source.to_string())
            .await;
    }

    async fn publish(
        &self,
        partition_key: String,
        critical: bool,
        payload: StreamPayload,
        source: String,
    ) {
        let envelope = {
            let mut state = self.state.lock().await;
            let event_seq = state.next_event_seq(&partition_key);
            StreamEnvelope {
                version: 1,
                event_seq,
                event_id: Uuid::new_v4().to_string(),
                source,
                partition_key,
                emitted_at_ms: now_ms(),
                critical,
                payload,
            }
        };

        self.publish_envelope(envelope).await;
    }

    pub async fn publish_envelope(&self, envelope: StreamEnvelope) {
        let subscribers = {
            let state = self.state.lock().await;
            state.subscribers.clone()
        };

        let mut disconnect_ids = Vec::new();
        for (connection_id, subscriber) in subscribers {
            if !matches_subscription(&subscriber.request, &envelope) {
                continue;
            }

            match subscriber.tx.try_send(envelope.clone()) {
                Ok(()) => {
                    self.metrics
                        .delivered
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_))
                    if !envelope.critical && envelope.is_depth_snapshot() =>
                {
                    self.metrics
                        .depth_dropped
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    self.metrics
                        .slow_consumer_disconnects
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    disconnect_ids.push(connection_id);
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    disconnect_ids.push(connection_id);
                }
            }
        }

        if !disconnect_ids.is_empty() {
            let mut state = self.state.lock().await;
            for connection_id in disconnect_ids {
                state.subscribers.remove(&connection_id);
            }
        }
    }

    fn authorized(&self, claims: &AuthClaims, request: &SubscriptionRequest) -> bool {
        match request.channel {
            ChannelKind::Market => claims.can_market_stream && request.market_id.is_some(),
            ChannelKind::User => {
                let requested = match &request.user_id {
                    Some(user_id) => user_id,
                    None => return false,
                };
                matches!(claims.user_id.as_deref(), Some(user_id) if user_id == requested)
            }
        }
    }
}

fn user_order_payload(
    user_id: &str,
    market_id: &str,
    outcome_id: &str,
    event: &Event,
) -> StreamPayload {
    match event {
        Event::OrderAccepted { order_id } => StreamPayload::UserOrderUpdate {
            user_id: user_id.to_string(),
            market_id: market_id.to_string(),
            outcome_id: outcome_id.to_string(),
            order_id: order_id.clone(),
            state: "accepted".into(),
            reason: None,
            qty_remaining: None,
        },
        Event::OrderRejected { order_id, reason } => StreamPayload::UserOrderUpdate {
            user_id: user_id.to_string(),
            market_id: market_id.to_string(),
            outcome_id: outcome_id.to_string(),
            order_id: order_id.clone(),
            state: "rejected".into(),
            reason: Some(reason.clone()),
            qty_remaining: None,
        },
        Event::OrderRested {
            order_id,
            qty_remaining,
        } => StreamPayload::UserOrderUpdate {
            user_id: user_id.to_string(),
            market_id: market_id.to_string(),
            outcome_id: outcome_id.to_string(),
            order_id: order_id.clone(),
            state: "rested".into(),
            reason: None,
            qty_remaining: Some(*qty_remaining),
        },
        Event::OrderCanceled { order_id } => StreamPayload::UserOrderUpdate {
            user_id: user_id.to_string(),
            market_id: market_id.to_string(),
            outcome_id: outcome_id.to_string(),
            order_id: order_id.clone(),
            state: "canceled".into(),
            reason: None,
            qty_remaining: None,
        },
        Event::OrderPartial {
            order_id,
            qty_remaining,
        } => StreamPayload::UserOrderUpdate {
            user_id: user_id.to_string(),
            market_id: market_id.to_string(),
            outcome_id: outcome_id.to_string(),
            order_id: order_id.clone(),
            state: "partial".into(),
            reason: None,
            qty_remaining: Some(*qty_remaining),
        },
        Event::OrderFilled { order_id } => StreamPayload::UserOrderUpdate {
            user_id: user_id.to_string(),
            market_id: market_id.to_string(),
            outcome_id: outcome_id.to_string(),
            order_id: order_id.clone(),
            state: "filled".into(),
            reason: None,
            qty_remaining: Some(0),
        },
        Event::TradeExecuted { taker_order_id, .. } => StreamPayload::UserOrderUpdate {
            user_id: user_id.to_string(),
            market_id: market_id.to_string(),
            outcome_id: outcome_id.to_string(),
            order_id: taker_order_id.clone(),
            state: "trade_executed".into(),
            reason: None,
            qty_remaining: None,
        },
    }
}

fn matches_subscription(request: &SubscriptionRequest, envelope: &StreamEnvelope) -> bool {
    match request.channel {
        ChannelKind::Market => match (&request.market_id, &envelope.payload) {
            (Some(request_market), StreamPayload::MarketTrade { market_id, .. }) => {
                market_id == request_market
            }
            (Some(request_market), StreamPayload::MarketTopOfBook { market_id, .. }) => {
                market_id == request_market
            }
            (Some(request_market), StreamPayload::MarketDepthSnapshot { market_id, .. }) => {
                request.include_depth_snapshots && market_id == request_market
            }
            _ => false,
        },
        ChannelKind::User => match (&request.user_id, &envelope.payload) {
            (Some(request_user), StreamPayload::UserOrderUpdate { user_id, .. }) => {
                user_id == request_user
            }
            (Some(request_user), StreamPayload::UserFill { user_id, .. }) => {
                user_id == request_user
            }
            (Some(request_user), StreamPayload::UserBalanceUpdate { user_id, .. }) => {
                user_id == request_user
            }
            _ => false,
        },
    }
}

fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i64,
        Err(_) => 0,
    }
}
