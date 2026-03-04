use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context};
use blake3::Hasher;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::book::BookState;
use crate::ledger::{FillIntent, LedgerAdapter, LedgerError, ReservationKindLocal};
use crate::persistence::Storage;
use crate::quality::QualityCollector;
use crate::risk::{check_place_risk, RiskLimits};
use crate::service::MatcherService;
use crate::types::{Command, CommandResult, Event};

#[derive(Debug, Clone)]
pub struct MarketTransfer {
    pub market_id: String,
    pub outcome_id: String,
    pub state_hash: String,
    pub command_log: Vec<Command>,
    pub book: BookState,
    pub processed_command_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RuntimeMetrics {
    pub shard_processed: Vec<u64>,
    pub shard_rejected: Vec<u64>,
    pub risk_rejects: u64,
}

struct RuntimeState {
    overrides: HashMap<String, usize>,
    frozen_markets: HashSet<String>,
}

impl RuntimeState {
    fn new() -> Self {
        Self {
            overrides: HashMap::new(),
            frozen_markets: HashSet::new(),
        }
    }
}

enum ShardMessage {
    Process {
        command: Command,
        response: oneshot::Sender<anyhow::Result<CommandResult>>,
    },
    GetBook {
        market_id: String,
        outcome_id: String,
        response: oneshot::Sender<Option<serde_json::Value>>,
    },
    ExportMarket {
        market_id: String,
        outcome_id: String,
        response: oneshot::Sender<anyhow::Result<Option<MarketTransfer>>>,
    },
    ImportMarket {
        transfer: MarketTransfer,
        response: oneshot::Sender<anyhow::Result<()>>,
    },
    RemoveMarket {
        market_id: String,
        outcome_id: String,
        response: oneshot::Sender<()>,
    },
}

pub struct ShardRuntime<L: LedgerAdapter + Clone + 'static> {
    shard_count: usize,
    risk_limits: RiskLimits,
    state: Arc<Mutex<RuntimeState>>,
    senders: Vec<mpsc::Sender<ShardMessage>>,
    processed: Vec<Arc<AtomicU64>>,
    rejected: Vec<Arc<AtomicU64>>,
    risk_rejects: Arc<AtomicU64>,
    quality: Arc<QualityCollector>,
    _ledger: std::marker::PhantomData<L>,
}

impl<L: LedgerAdapter + Clone + 'static> ShardRuntime<L> {
    pub fn new(
        shard_count: usize,
        base_data_dir: impl Into<PathBuf>,
        ledger: L,
        snapshot_every: u64,
        risk_limits: RiskLimits,
    ) -> anyhow::Result<Self> {
        if shard_count == 0 {
            return Err(anyhow!("shard_count must be > 0"));
        }

        let base_data_dir = base_data_dir.into();
        let state = Arc::new(Mutex::new(RuntimeState::new()));
        let risk_rejects = Arc::new(AtomicU64::new(0));
        let quality = Arc::new(QualityCollector::new());
        let mut senders = Vec::with_capacity(shard_count);
        let mut processed = Vec::with_capacity(shard_count);
        let mut rejected = Vec::with_capacity(shard_count);

        for shard_id in 0..shard_count {
            let (tx, mut rx) = mpsc::channel::<ShardMessage>(512);
            senders.push(tx);

            let shard_processed = Arc::new(AtomicU64::new(0));
            let shard_rejected = Arc::new(AtomicU64::new(0));
            processed.push(shard_processed.clone());
            rejected.push(shard_rejected.clone());

            let shard_dir = base_data_dir.join(format!("shard-{shard_id}"));
            let shard_ledger = ledger.clone();
            let shard_limits = risk_limits.clone();
            let shard_risk_rejects = risk_rejects.clone();

            tokio::spawn(async move {
                let mut services: HashMap<(String, String), MatcherService<L>> = HashMap::new();

                while let Some(msg) = rx.recv().await {
                    match msg {
                        ShardMessage::Process { command, response } => {
                            let ctx = ShardProcessCtx {
                                shard_dir: &shard_dir,
                                ledger: shard_ledger.clone(),
                                snapshot_every,
                                risk_limits: &shard_limits,
                                risk_rejects: &shard_risk_rejects,
                                shard_processed: &shard_processed,
                                shard_rejected: &shard_rejected,
                            };
                            let result = process_on_shard(&mut services, command, ctx).await;
                            let _ = response.send(result);
                        }
                        ShardMessage::GetBook {
                            market_id,
                            outcome_id,
                            response,
                        } => {
                            let view = services
                                .get(&(market_id, outcome_id))
                                .map(|service| serde_json::json!(service.book.view()));
                            let _ = response.send(view);
                        }
                        ShardMessage::ExportMarket {
                            market_id,
                            outcome_id,
                            response,
                        } => {
                            let out = if let Some(service) =
                                services.get(&(market_id.clone(), outcome_id.clone()))
                            {
                                let (book, processed_command_ids) = service.export_snapshot();
                                match service.read_all_commands().await {
                                    Ok(command_log) => Ok(Some(MarketTransfer {
                                        market_id,
                                        outcome_id,
                                        state_hash: service.state_hash(),
                                        command_log,
                                        book,
                                        processed_command_ids,
                                    })),
                                    Err(err) => Err(err.context("read command log for migration")),
                                }
                            } else {
                                Ok(None)
                            };
                            let _ = response.send(out);
                        }
                        ShardMessage::ImportMarket { transfer, response } => {
                            let out = import_on_shard(
                                &mut services,
                                shard_dir.as_path(),
                                shard_ledger.clone(),
                                snapshot_every,
                                transfer,
                            )
                            .await;
                            let _ = response.send(out);
                        }
                        ShardMessage::RemoveMarket {
                            market_id,
                            outcome_id,
                            response,
                        } => {
                            services.remove(&(market_id, outcome_id));
                            let _ = response.send(());
                        }
                    }
                }
            });
        }

        Ok(Self {
            shard_count,
            risk_limits,
            state,
            senders,
            processed,
            rejected,
            risk_rejects,
            quality,
            _ledger: std::marker::PhantomData,
        })
    }

    pub fn shard_count(&self) -> usize {
        self.shard_count
    }

    pub fn route_market(&self, market_id: &str) -> usize {
        stable_shard_for_market(market_id, self.shard_count)
    }

    pub async fn route_with_overrides(&self, market_id: &str) -> usize {
        let state = self.state.lock().await;
        state
            .overrides
            .get(market_id)
            .copied()
            .unwrap_or_else(|| self.route_market(market_id))
    }

    pub async fn process(&self, command: Command) -> anyhow::Result<CommandResult> {
        let started_at = std::time::Instant::now();
        let (market_id, order_id, command_id) = match &command {
            Command::Place(place) => (
                place.market_id.as_str(),
                place.order_id.clone(),
                place.command_id.clone(),
            ),
            Command::Cancel(cancel) => (
                cancel.market_id.as_str(),
                cancel.order_id.clone(),
                cancel.command_id.clone(),
            ),
        };

        if market_id.is_empty() {
            let out = CommandResult {
                command_id,
                events: vec![Event::OrderRejected {
                    order_id,
                    reason: "invalid_market_metadata".into(),
                }],
            };
            self.quality
                .record_command(
                    &command,
                    None,
                    started_at.elapsed().as_millis() as u64,
                    &out,
                )
                .await;
            return Ok(out);
        }

        let shard = {
            let state = self.state.lock().await;
            if state.frozen_markets.contains(market_id) {
                let out = CommandResult {
                    command_id,
                    events: vec![Event::OrderRejected {
                        order_id,
                        reason: "market_migrating".into(),
                    }],
                };
                self.quality
                    .record_command(
                        &command,
                        None,
                        started_at.elapsed().as_millis() as u64,
                        &out,
                    )
                    .await;
                return Ok(out);
            }
            state
                .overrides
                .get(market_id)
                .copied()
                .unwrap_or_else(|| self.route_market(market_id))
        };

        let command_for_quality = command.clone();
        let out = self.dispatch_to_shard(shard, command).await?;
        self.quality
            .record_command(
                &command_for_quality,
                Some(shard),
                started_at.elapsed().as_millis() as u64,
                &out,
            )
            .await;
        Ok(out)
    }

    pub async fn process_on_shard(
        &self,
        shard: usize,
        command: Command,
    ) -> anyhow::Result<CommandResult> {
        if shard >= self.shard_count {
            return Err(anyhow!("shard {shard} out of range"));
        }
        self.dispatch_to_shard(shard, command).await
    }

    pub async fn get_book(
        &self,
        market_id: String,
        outcome_id: String,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let shard = self.route_with_overrides(&market_id).await;
        let (tx, rx) = oneshot::channel();
        self.senders[shard]
            .send(ShardMessage::GetBook {
                market_id,
                outcome_id,
                response: tx,
            })
            .await
            .map_err(|_| anyhow!("shard {shard} queue closed"))?;
        rx.await
            .map_err(|_| anyhow!("shard {shard} dropped response"))
    }

    pub async fn migrate_market(
        &self,
        market_id: String,
        outcome_id: String,
        target_shard: usize,
    ) -> anyhow::Result<()> {
        if target_shard >= self.shard_count {
            return Err(anyhow!("target shard out of range"));
        }

        let source_shard = self.route_with_overrides(&market_id).await;
        if source_shard == target_shard {
            return Ok(());
        }

        {
            let mut state = self.state.lock().await;
            state.frozen_markets.insert(market_id.clone());
        }

        let (tx, rx) = oneshot::channel();
        self.senders[source_shard]
            .send(ShardMessage::ExportMarket {
                market_id: market_id.clone(),
                outcome_id: outcome_id.clone(),
                response: tx,
            })
            .await
            .map_err(|_| anyhow!("source shard queue closed"))?;
        let transfer = rx
            .await
            .map_err(|_| anyhow!("source shard dropped response"))?
            .context("export market failed")?;

        let transfer = transfer.ok_or_else(|| anyhow!("market not found on source shard"))?;
        let (tx2, rx2) = oneshot::channel();
        self.senders[target_shard]
            .send(ShardMessage::ImportMarket {
                transfer,
                response: tx2,
            })
            .await
            .map_err(|_| anyhow!("target shard queue closed"))?;
        rx2.await
            .map_err(|_| anyhow!("target shard dropped response"))?
            .context("import market failed")?;

        let (tx3, rx3) = oneshot::channel();
        self.senders[source_shard]
            .send(ShardMessage::RemoveMarket {
                market_id: market_id.clone(),
                outcome_id,
                response: tx3,
            })
            .await
            .map_err(|_| anyhow!("source shard queue closed"))?;
        let _ = rx3.await;

        let mut state = self.state.lock().await;
        state.overrides.insert(market_id.clone(), target_shard);
        state.frozen_markets.remove(&market_id);
        Ok(())
    }

    pub fn metrics(&self) -> RuntimeMetrics {
        RuntimeMetrics {
            shard_processed: self
                .processed
                .iter()
                .map(|v| v.load(Ordering::Relaxed))
                .collect(),
            shard_rejected: self
                .rejected
                .iter()
                .map(|v| v.load(Ordering::Relaxed))
                .collect(),
            risk_rejects: self.risk_rejects.load(Ordering::Relaxed),
        }
    }

    pub fn risk_limits(&self) -> &RiskLimits {
        &self.risk_limits
    }

    pub fn quality(&self) -> Arc<QualityCollector> {
        self.quality.clone()
    }

    async fn dispatch_to_shard(
        &self,
        shard: usize,
        command: Command,
    ) -> anyhow::Result<CommandResult> {
        let (tx, rx) = oneshot::channel();
        self.senders[shard]
            .send(ShardMessage::Process {
                command,
                response: tx,
            })
            .await
            .map_err(|_| anyhow!("shard {shard} queue closed"))?;
        rx.await
            .map_err(|_| anyhow!("shard {shard} dropped response"))?
    }
}

struct ShardProcessCtx<'a, L: LedgerAdapter + Clone + 'static> {
    shard_dir: &'a Path,
    ledger: L,
    snapshot_every: u64,
    risk_limits: &'a RiskLimits,
    risk_rejects: &'a Arc<AtomicU64>,
    shard_processed: &'a Arc<AtomicU64>,
    shard_rejected: &'a Arc<AtomicU64>,
}

async fn process_on_shard<L: LedgerAdapter + Clone + 'static>(
    services: &mut HashMap<(String, String), MatcherService<L>>,
    command: Command,
    ctx: ShardProcessCtx<'_, L>,
) -> anyhow::Result<CommandResult> {
    let (market_id, outcome_id) = match &command {
        Command::Place(place) => (place.market_id.clone(), place.outcome_id.clone()),
        Command::Cancel(cancel) => (cancel.market_id.clone(), cancel.outcome_id.clone()),
    };

    if market_id.is_empty() || outcome_id.is_empty() {
        let (command_id, order_id) = match &command {
            Command::Place(place) => (place.command_id.clone(), place.order_id.clone()),
            Command::Cancel(cancel) => (cancel.command_id.clone(), cancel.order_id.clone()),
        };
        ctx.shard_rejected.fetch_add(1, Ordering::Relaxed);
        return Ok(CommandResult {
            command_id,
            events: vec![Event::OrderRejected {
                order_id,
                reason: "invalid_market_metadata".into(),
            }],
        });
    }

    let key = (market_id.clone(), outcome_id.clone());
    if !services.contains_key(&key) {
        let storage = Storage::new(ctx.shard_dir.join(sanitize_key(&market_id, &outcome_id)));
        let service = MatcherService::new(
            market_id,
            outcome_id,
            storage,
            ctx.ledger.clone(),
            ctx.snapshot_every,
        )
        .await
        .context("create market matcher")?;
        services.insert(key.clone(), service);
    }

    let service = services
        .get_mut(&key)
        .ok_or_else(|| anyhow!("service missing after creation"))?;

    if let Command::Place(place) = &command {
        if let Err(reason) = check_place_risk(&service.book, place, ctx.risk_limits) {
            ctx.risk_rejects.fetch_add(1, Ordering::Relaxed);
            ctx.shard_rejected.fetch_add(1, Ordering::Relaxed);
            return Ok(CommandResult {
                command_id: place.command_id.clone(),
                events: vec![Event::OrderRejected {
                    order_id: place.order_id.clone(),
                    reason,
                }],
            });
        }
    }

    let out = service.process(command).await?;
    if out
        .events
        .iter()
        .any(|event| matches!(event, Event::OrderRejected { .. }))
    {
        ctx.shard_rejected.fetch_add(1, Ordering::Relaxed);
    } else {
        ctx.shard_processed.fetch_add(1, Ordering::Relaxed);
    }
    Ok(out)
}

async fn import_on_shard<L: LedgerAdapter + Clone + 'static>(
    services: &mut HashMap<(String, String), MatcherService<L>>,
    shard_dir: &Path,
    ledger: L,
    snapshot_every: u64,
    transfer: MarketTransfer,
) -> anyhow::Result<()> {
    let verify_dir = std::env::temp_dir().join(format!(
        "matcher-migration-verify-{}-{}",
        sanitize_component(&transfer.market_id),
        sanitize_component(&transfer.outcome_id)
    ));
    let verify_storage = Storage::new(verify_dir);
    let mut verifier = MatcherService::new(
        transfer.market_id.clone(),
        transfer.outcome_id.clone(),
        verify_storage,
        NoopLedger,
        0,
    )
    .await
    .context("create migration verifier")?;

    for command in transfer.command_log.iter().cloned() {
        let _ = verifier
            .replay(command)
            .await
            .context("replay command during migration")?;
    }

    if verifier.state_hash() != transfer.state_hash {
        return Err(anyhow!(
            "migration hash mismatch: source={} target={}",
            transfer.state_hash,
            verifier.state_hash()
        ));
    }

    let storage =
        Storage::new(shard_dir.join(sanitize_key(&transfer.market_id, &transfer.outcome_id)));
    let service = MatcherService::from_snapshot(
        storage,
        ledger,
        snapshot_every,
        transfer.book,
        transfer.processed_command_ids,
    )
    .await
    .context("materialize migrated market snapshot")?;

    services.insert((transfer.market_id, transfer.outcome_id), service);
    Ok(())
}

fn sanitize_key(market_id: &str, outcome_id: &str) -> String {
    format!(
        "{}__{}",
        sanitize_component(market_id),
        sanitize_component(outcome_id)
    )
}

fn sanitize_component(component: &str) -> String {
    component
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '_',
        })
        .collect()
}

pub fn stable_shard_for_market(market_id: &str, shard_count: usize) -> usize {
    let mut hasher = Hasher::new();
    hasher.update(market_id.as_bytes());
    let hash = hasher.finalize();
    let bytes = hash.as_bytes();
    let mut buf = [0_u8; 8];
    buf.copy_from_slice(&bytes[..8]);
    (u64::from_le_bytes(buf) as usize) % shard_count
}

#[derive(Clone)]
struct NoopLedger;

#[tonic::async_trait]
impl LedgerAdapter for NoopLedger {
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
