use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::sync::RwLock;

use crate::ledger::LedgerAdapter;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MarketLifecycleState {
    Open,
    Proposed,
    DisputeWindow,
    InReview,
    Finalized,
    Settled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceBands {
    pub min_tick: i64,
    pub max_tick: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeConfig {
    pub taker_fee_bps: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisputeRecord {
    pub user_id: String,
    pub bond_czk: i64,
    pub filed_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketRecord {
    pub market_id: String,
    pub outcomes: Vec<String>,
    pub sources: Vec<String>,
    pub criteria: String,
    pub dispute_window_secs: i64,
    pub price_bands: PriceBands,
    pub fee_config: FeeConfig,
    pub state: MarketLifecycleState,
    pub proposed_outcome_id: Option<String>,
    pub finalized_outcome_id: Option<String>,
    pub dispute_window_open_at_ms: Option<i64>,
    pub dispute_window_close_at_ms: Option<i64>,
    pub dispute: Option<DisputeRecord>,
    pub settlement_idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub market_id: String,
    pub action: String,
    pub from_state: Option<MarketLifecycleState>,
    pub to_state: MarketLifecycleState,
    pub actor: String,
    pub at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PersistedAdminState {
    markets: BTreeMap<String, MarketRecord>,
    audit_events: Vec<AuditEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateMarketRequest {
    pub market_id: String,
    pub outcomes: Vec<String>,
    pub sources: Vec<String>,
    pub criteria: String,
    pub dispute_window_secs: i64,
    pub price_bands: PriceBands,
    pub fee_config: FeeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposeResolutionRequest {
    pub outcome_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDisputeRequest {
    pub user_id: String,
    pub bond_czk: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalizeResolutionRequest {
    pub outcome_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettleMarketRequest {
    pub idempotency_key: String,
    pub chunk_size: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettleMarketOutcome {
    pub idempotent: bool,
}

#[derive(Clone)]
pub struct AdminAuthorizer {
    token_roles: Arc<HashMap<String, String>>,
}

impl AdminAuthorizer {
    pub fn from_env() -> Self {
        let raw = std::env::var("ADMIN_API_TOKENS")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("ADMIN_API_TOKEN")
                    .ok()
                    .map(|token| format!("{}:admin", token.trim()))
            })
            .unwrap_or_else(|| "admin-token:admin".to_string());

        let mut token_roles = HashMap::new();
        for entry in raw.split(',') {
            let trimmed = entry.trim();
            if trimmed.is_empty() {
                continue;
            }
            let mut parts = trimmed.splitn(2, ':');
            let token = parts.next().unwrap_or_default().trim();
            let role = parts.next().unwrap_or("admin").trim();
            if !token.is_empty() && !role.is_empty() {
                token_roles.insert(token.to_string(), role.to_string());
            }
        }

        if token_roles.is_empty() {
            token_roles.insert("admin-token".to_string(), "admin".to_string());
        }

        Self {
            token_roles: Arc::new(token_roles),
        }
    }

    pub fn role_for_token(&self, token: &str) -> Option<&str> {
        self.token_roles.get(token).map(String::as_str)
    }
}

#[derive(Clone)]
pub struct AdminController<L: LedgerAdapter + Clone + 'static> {
    ledger: L,
    state: Arc<RwLock<PersistedAdminState>>,
    state_path: Arc<PathBuf>,
    now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl<L: LedgerAdapter + Clone + 'static> AdminController<L> {
    pub async fn new(data_dir: impl AsRef<Path>, ledger: L) -> anyhow::Result<Self> {
        let admin_dir = data_dir.as_ref().join("admin");
        fs::create_dir_all(&admin_dir)
            .await
            .context("create admin data directory")?;
        let state_path = admin_dir.join("markets.json");
        let persisted = load_state(&state_path).await?;
        Ok(Self {
            ledger,
            state: Arc::new(RwLock::new(persisted)),
            state_path: Arc::new(state_path),
            now_ms: Arc::new(current_time_ms),
        })
    }

    pub fn with_clock(self, now_ms: Arc<dyn Fn() -> i64 + Send + Sync>) -> Self {
        Self { now_ms, ..self }
    }

    pub async fn create_market(
        &self,
        actor: &str,
        request: CreateMarketRequest,
    ) -> anyhow::Result<MarketRecord> {
        validate_create_request(&request)?;

        let mut state = self.state.write().await;
        if state.markets.contains_key(&request.market_id) {
            bail!("market already exists");
        }

        let market = MarketRecord {
            market_id: request.market_id.clone(),
            outcomes: request.outcomes,
            sources: request.sources,
            criteria: request.criteria,
            dispute_window_secs: request.dispute_window_secs,
            price_bands: request.price_bands,
            fee_config: request.fee_config,
            state: MarketLifecycleState::Open,
            proposed_outcome_id: None,
            finalized_outcome_id: None,
            dispute_window_open_at_ms: None,
            dispute_window_close_at_ms: None,
            dispute: None,
            settlement_idempotency_key: None,
        };

        let now = (self.now_ms)();
        state
            .markets
            .insert(request.market_id.clone(), market.clone());
        state.audit_events.push(AuditEvent {
            market_id: request.market_id,
            action: "create_market".into(),
            from_state: None,
            to_state: MarketLifecycleState::Open,
            actor: actor.to_string(),
            at_ms: now,
        });
        persist_state(&self.state_path, &state).await?;
        Ok(market)
    }

    pub async fn propose_resolution(
        &self,
        actor: &str,
        market_id: &str,
        request: ProposeResolutionRequest,
    ) -> anyhow::Result<MarketRecord> {
        let mut state = self.state.write().await;
        let now = (self.now_ms)();
        let (market_snapshot, events) = {
            let market = state
                .markets
                .get_mut(market_id)
                .ok_or_else(|| anyhow!("market not found"))?;

            if market.state != MarketLifecycleState::Open {
                bail!("market is not open");
            }
            if !market
                .outcomes
                .iter()
                .any(|outcome| outcome == &request.outcome_id)
            {
                bail!("proposed outcome is not part of market outcomes");
            }

            let first_event = transition_market(
                market,
                actor,
                "resolution_proposed",
                now,
                MarketLifecycleState::Proposed,
            );
            market.proposed_outcome_id = Some(request.outcome_id);
            market.dispute_window_open_at_ms = Some(now);
            market.dispute_window_close_at_ms = Some(
                now.checked_add(market.dispute_window_secs.saturating_mul(1_000))
                    .ok_or_else(|| anyhow!("dispute window overflow"))?,
            );
            let second_event = transition_market(
                market,
                actor,
                "dispute_window_opened",
                now,
                MarketLifecycleState::DisputeWindow,
            );
            (market.clone(), vec![first_event, second_event])
        };
        state.audit_events.extend(events);
        persist_state(&self.state_path, &state).await?;
        Ok(market_snapshot)
    }

    pub async fn file_dispute(
        &self,
        actor: &str,
        market_id: &str,
        request: FileDisputeRequest,
    ) -> anyhow::Result<MarketRecord> {
        if request.bond_czk <= 0 {
            bail!("dispute bond must be positive");
        }

        let mut state = self.state.write().await;
        let now = (self.now_ms)();
        let (market_snapshot, event) = {
            let market = state
                .markets
                .get_mut(market_id)
                .ok_or_else(|| anyhow!("market not found"))?;
            if market.state != MarketLifecycleState::DisputeWindow {
                bail!("market not in dispute window");
            }

            let opens = market
                .dispute_window_open_at_ms
                .ok_or_else(|| anyhow!("missing dispute window start"))?;
            let closes = market
                .dispute_window_close_at_ms
                .ok_or_else(|| anyhow!("missing dispute window close"))?;

            if now < opens || now > closes {
                bail!("dispute window closed");
            }

            market.dispute = Some(DisputeRecord {
                user_id: request.user_id,
                bond_czk: request.bond_czk,
                filed_at_ms: now,
            });
            let event = transition_market(
                market,
                actor,
                "dispute_filed",
                now,
                MarketLifecycleState::InReview,
            );
            (market.clone(), event)
        };
        state.audit_events.push(event);
        persist_state(&self.state_path, &state).await?;
        Ok(market_snapshot)
    }

    pub async fn finalize_resolution(
        &self,
        actor: &str,
        market_id: &str,
        request: FinalizeResolutionRequest,
    ) -> anyhow::Result<MarketRecord> {
        let mut state = self.state.write().await;
        let now = (self.now_ms)();
        let (market_snapshot, event) = {
            let market = state
                .markets
                .get_mut(market_id)
                .ok_or_else(|| anyhow!("market not found"))?;

            if !market
                .outcomes
                .iter()
                .any(|outcome| outcome == &request.outcome_id)
            {
                bail!("final outcome is not part of market outcomes");
            }

            match market.state {
                MarketLifecycleState::DisputeWindow => {
                    let closes = market
                        .dispute_window_close_at_ms
                        .ok_or_else(|| anyhow!("missing dispute window close"))?;
                    if now < closes {
                        bail!("cannot finalize before dispute window closes");
                    }
                    if market.dispute.is_some() {
                        bail!("market has active dispute in review");
                    }
                }
                MarketLifecycleState::InReview => {}
                _ => bail!("market is not ready for finalization"),
            }

            market.finalized_outcome_id = Some(request.outcome_id);
            let event = transition_market(
                market,
                actor,
                "resolution_finalized",
                now,
                MarketLifecycleState::Finalized,
            );
            (market.clone(), event)
        };
        state.audit_events.push(event);
        persist_state(&self.state_path, &state).await?;
        Ok(market_snapshot)
    }

    pub async fn settle_market(
        &self,
        actor: &str,
        market_id: &str,
        request: SettleMarketRequest,
    ) -> anyhow::Result<(MarketRecord, SettleMarketOutcome)> {
        if request.idempotency_key.trim().is_empty() {
            bail!("idempotency_key is required");
        }

        let settled_idempotently = {
            let state = self.state.read().await;
            let market = state
                .markets
                .get(market_id)
                .ok_or_else(|| anyhow!("market not found"))?;
            if market.state == MarketLifecycleState::Settled {
                if market.settlement_idempotency_key.as_deref()
                    == Some(request.idempotency_key.as_str())
                {
                    Some(market.clone())
                } else {
                    bail!("market already settled with different idempotency key");
                }
            } else {
                None
            }
        };
        if let Some(market) = settled_idempotently {
            return Ok((market, SettleMarketOutcome { idempotent: true }));
        }

        let (winning_outcome_id, chunk_size) = {
            let state = self.state.read().await;
            let market = state
                .markets
                .get(market_id)
                .ok_or_else(|| anyhow!("market not found"))?;
            if market.state != MarketLifecycleState::Finalized {
                bail!("market must be finalized before settlement");
            }
            let winning = market
                .finalized_outcome_id
                .clone()
                .ok_or_else(|| anyhow!("missing finalized outcome"))?;
            (winning, request.chunk_size.unwrap_or(250))
        };

        self.ledger
            .settle_market(
                &format!("settle:{market_id}:{}", request.idempotency_key),
                &request.idempotency_key,
                market_id,
                &winning_outcome_id,
                chunk_size,
            )
            .await
            .map_err(|err| anyhow!("ledger settlement failed: {err}"))?;

        let mut state = self.state.write().await;
        let now = (self.now_ms)();
        let (market_snapshot, event) = {
            let market = state
                .markets
                .get_mut(market_id)
                .ok_or_else(|| anyhow!("market not found"))?;
            if market.state == MarketLifecycleState::Settled
                && market.settlement_idempotency_key.as_deref()
                    == Some(request.idempotency_key.as_str())
            {
                return Ok((market.clone(), SettleMarketOutcome { idempotent: true }));
            }
            if market.state != MarketLifecycleState::Finalized {
                bail!("market state changed before settlement");
            }

            market.settlement_idempotency_key = Some(request.idempotency_key);
            let event = transition_market(
                market,
                actor,
                "market_settled",
                now,
                MarketLifecycleState::Settled,
            );
            (market.clone(), event)
        };
        state.audit_events.push(event);
        persist_state(&self.state_path, &state).await?;
        Ok((market_snapshot, SettleMarketOutcome { idempotent: false }))
    }

    pub async fn get_market(&self, market_id: &str) -> Option<MarketRecord> {
        let state = self.state.read().await;
        state.markets.get(market_id).cloned()
    }
}

fn transition_market(
    market: &mut MarketRecord,
    actor: &str,
    action: &str,
    at_ms: i64,
    to_state: MarketLifecycleState,
) -> AuditEvent {
    let from_state = Some(market.state.clone());
    market.state = to_state.clone();
    AuditEvent {
        market_id: market.market_id.clone(),
        action: action.to_string(),
        from_state,
        to_state,
        actor: actor.to_string(),
        at_ms,
    }
}

fn validate_create_request(request: &CreateMarketRequest) -> anyhow::Result<()> {
    if request.market_id.trim().is_empty() {
        bail!("market_id is required");
    }
    if request.criteria.trim().is_empty() {
        bail!("criteria is required");
    }
    if request.outcomes.len() < 2 {
        bail!("at least two outcomes are required");
    }
    if request.sources.is_empty() {
        bail!("at least one source is required");
    }
    if request.dispute_window_secs < 0 {
        bail!("dispute_window_secs must be non-negative");
    }
    if request.price_bands.min_tick < 0
        || request.price_bands.max_tick < request.price_bands.min_tick
    {
        bail!("invalid price bands");
    }
    if request.fee_config.taker_fee_bps < 0 {
        bail!("invalid fee config");
    }
    Ok(())
}

fn current_time_ms() -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    now.as_millis() as i64
}

async fn load_state(path: &Path) -> anyhow::Result<PersistedAdminState> {
    match fs::read(path).await {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).context("decode admin state")?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(PersistedAdminState::default())
        }
        Err(err) => Err(err.into()),
    }
}

async fn persist_state(path: &Path, state: &PersistedAdminState) -> anyhow::Result<()> {
    let json = serde_json::to_vec_pretty(state).context("encode admin state")?;
    fs::write(path, json).await.context("write admin state")
}
