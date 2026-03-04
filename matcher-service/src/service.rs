use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Context};
use tracing::warn;
use uuid::Uuid;

use crate::book::BookState;
use crate::ledger::{FillIntent, LedgerAdapter, LedgerError, ReservationKindLocal};
use crate::persistence::Storage;
use crate::types::{CancelOrder, Command, CommandResult, Event, PlaceOrder, Side};

pub struct MatcherService<L: LedgerAdapter> {
    pub book: BookState,
    storage: Storage,
    ledger: L,
    market_id: String,
    outcome_id: String,
    processed: HashMap<String, Vec<Event>>,
    processed_set: HashSet<String>,
    snapshot_every: u64,
}

impl<L: LedgerAdapter> MatcherService<L> {
    pub async fn new(
        market_id: String,
        outcome_id: String,
        storage: Storage,
        ledger: L,
        snapshot_every: u64,
    ) -> anyhow::Result<Self> {
        let loaded = storage
            .load(market_id.clone(), outcome_id.clone())
            .await
            .context("load storage")?;

        let mut service = Self {
            book: loaded.book,
            storage,
            ledger,
            market_id,
            outcome_id,
            processed: HashMap::new(),
            processed_set: loaded.processed_command_ids.iter().cloned().collect(),
            snapshot_every,
        };

        for cmd in loaded.replay_commands {
            let _ = service
                .process_internal(cmd, false)
                .await
                .map_err(|e| anyhow!("replay failed: {e}"))?;
        }

        Ok(service)
    }

    pub async fn process(&mut self, command: Command) -> anyhow::Result<CommandResult> {
        self.process_internal(command, true).await
    }

    pub async fn replay(&mut self, command: Command) -> anyhow::Result<CommandResult> {
        self.process_internal(command, false).await
    }

    pub fn state_hash(&self) -> String {
        self.book.state_hash()
    }

    pub fn market_id(&self) -> &str {
        &self.market_id
    }

    pub fn outcome_id(&self) -> &str {
        &self.outcome_id
    }

    pub fn export_snapshot(&self) -> (BookState, Vec<String>) {
        let processed_ids: Vec<String> = self.processed_set.iter().cloned().collect();
        (self.book.clone(), processed_ids)
    }

    pub async fn read_all_commands(&self) -> anyhow::Result<Vec<Command>> {
        self.storage.read_all_commands().await
    }

    pub async fn from_snapshot(
        storage: Storage,
        ledger: L,
        snapshot_every: u64,
        book: BookState,
        processed_command_ids: Vec<String>,
    ) -> anyhow::Result<Self> {
        storage
            .write_snapshot(&book, &processed_command_ids)
            .await
            .context("write imported snapshot")?;

        Ok(Self {
            market_id: book.market_id.clone(),
            outcome_id: book.outcome_id.clone(),
            book,
            storage,
            ledger,
            processed: HashMap::new(),
            processed_set: processed_command_ids.into_iter().collect(),
            snapshot_every,
        })
    }

    async fn process_internal(
        &mut self,
        command: Command,
        persist: bool,
    ) -> anyhow::Result<CommandResult> {
        let command_id = match &command {
            Command::Place(p) => p.command_id.clone(),
            Command::Cancel(c) => c.command_id.clone(),
        };

        let (market_id, outcome_id) = match &command {
            Command::Place(p) => (&p.market_id, &p.outcome_id),
            Command::Cancel(c) => (&c.market_id, &c.outcome_id),
        };
        if market_id != &self.market_id || outcome_id != &self.outcome_id {
            return Ok(CommandResult {
                command_id,
                events: vec![Event::OrderRejected {
                    order_id: match &command {
                        Command::Place(p) => p.order_id.clone(),
                        Command::Cancel(c) => c.order_id.clone(),
                    },
                    reason: "market/outcome mismatch".into(),
                }],
            });
        }

        if let Some(events) = self.processed.get(&command_id) {
            return Ok(CommandResult {
                command_id,
                events: events.clone(),
            });
        }

        if self.processed_set.contains(&command_id) {
            return Ok(CommandResult {
                command_id,
                events: vec![],
            });
        }

        let seq = self.book.next_command_seq();
        if persist {
            self.storage
                .append_command(seq, &command)
                .await
                .context("append command")?;
        }

        let events = match &command {
            Command::Place(cmd) => self.handle_place(cmd).await,
            Command::Cancel(cmd) => self.handle_cancel(cmd).await,
        };

        self.processed.insert(command_id.clone(), events.clone());
        self.processed_set.insert(command_id.clone());

        if persist
            && self.snapshot_every != 0
            && self.book.command_seq.is_multiple_of(self.snapshot_every)
        {
            let processed_ids: Vec<String> = self.processed_set.iter().cloned().collect();
            self.storage
                .write_snapshot(&self.book, &processed_ids)
                .await
                .context("write snapshot")?;
        }

        Ok(CommandResult { command_id, events })
    }

    async fn handle_place(&mut self, cmd: &PlaceOrder) -> Vec<Event> {
        if cmd.qty <= 0 || cmd.limit_price < 0 {
            return vec![Event::OrderRejected {
                order_id: cmd.order_id.clone(),
                reason: "invalid qty/price".into(),
            }];
        }
        if Uuid::parse_str(&cmd.client_order_id).is_err() {
            return vec![Event::OrderRejected {
                order_id: cmd.order_id.clone(),
                reason: "invalid client_order_id".into(),
            }];
        }
        if self.book.orders.contains_key(&cmd.order_id) {
            return vec![Event::OrderRejected {
                order_id: cmd.order_id.clone(),
                reason: "order_id already exists".into(),
            }];
        }

        let reserve = match cmd.side {
            Side::Buy => BookState::reserve_buy_czk(cmd.limit_price, cmd.qty),
            Side::Sell => BookState::reserve_sell_collateral_czk(cmd.limit_price, cmd.qty),
        };

        let Some(reserve_czk) = reserve else {
            return vec![Event::OrderRejected {
                order_id: cmd.order_id.clone(),
                reason: "reserve overflow".into(),
            }];
        };

        let reservation_kind = match cmd.side {
            Side::Buy => ReservationKindLocal::Buy,
            Side::Sell => ReservationKindLocal::ShortCollateral,
        };

        if let Err(err) = self
            .ledger
            .reserve_for_order(
                &format!("{}:reserve", cmd.command_id),
                &cmd.market_id,
                &cmd.outcome_id,
                &cmd.user_id,
                &cmd.order_id,
                reservation_kind,
                reserve_czk,
            )
            .await
        {
            return vec![Event::OrderRejected {
                order_id: cmd.order_id.clone(),
                reason: format_ledger_error(err),
            }];
        }

        let match_result = self.book.place_and_match(cmd);

        for fill in &match_result.fills {
            let intent = FillIntent {
                fill_id: fill.fill_id.clone(),
                maker_user_id: fill.maker_user_id.clone(),
                maker_order_id: fill.maker_order_id.clone(),
                taker_user_id: cmd.user_id.clone(),
                taker_order_id: fill.taker_order_id.clone(),
                market_id: self.market_id.clone(),
                outcome_id: self.outcome_id.clone(),
                qty: fill.qty,
                price_czk: fill.price,
                taker_side: cmd.side,
                fee_bps: 25,
            };

            if let Err(err) = self.ledger.apply_fill(intent).await {
                warn!("apply_fill failed for {}: {}", fill.fill_id, err);
            }
        }

        let remaining = match_result.taker_qty_remaining;

        if remaining == 0 {
            if let Err(err) = self
                .ledger
                .release_reservation(&format!("{}:release-final", cmd.command_id), &cmd.order_id)
                .await
            {
                warn!("release reservation failed for {}: {}", cmd.order_id, err);
            }
        } else {
            let required_remaining = match cmd.side {
                Side::Buy => BookState::reserve_buy_czk(cmd.limit_price, remaining),
                Side::Sell => BookState::reserve_sell_collateral_czk(cmd.limit_price, remaining),
            }
            .unwrap_or(0);

            let delta = required_remaining - reserve_czk;
            if delta != 0 {
                if let Err(err) = self
                    .ledger
                    .adjust_reservation(&format!("{}:adjust", cmd.command_id), &cmd.order_id, delta)
                    .await
                {
                    warn!("adjust reservation failed for {}: {}", cmd.order_id, err);
                }
            }

            if cmd.order_type == crate::types::OrderType::Ioc {
                if let Err(err) = self
                    .ledger
                    .release_reservation(&format!("{}:release-ioc", cmd.command_id), &cmd.order_id)
                    .await
                {
                    warn!(
                        "release reservation failed for IOC {}: {}",
                        cmd.order_id, err
                    );
                }
            }
        }

        match_result.events
    }

    async fn handle_cancel(&mut self, cmd: &CancelOrder) -> Vec<Event> {
        let Some(evt) = self.book.cancel_order(&cmd.user_id, &cmd.order_id) else {
            return vec![Event::OrderRejected {
                order_id: cmd.order_id.clone(),
                reason: "order not found or not owned by user".into(),
            }];
        };

        if let Err(err) = self
            .ledger
            .release_reservation(&format!("{}:release-cancel", cmd.command_id), &cmd.order_id)
            .await
        {
            warn!("release reservation on cancel failed: {}", err);
        }

        vec![evt]
    }
}

fn format_ledger_error(err: LedgerError) -> String {
    match err {
        LedgerError::Rejected(r) => format!("ledger_rejected:{r}"),
        LedgerError::Transient(r) => format!("ledger_transient:{r}"),
        LedgerError::Rpc(r) => format!("ledger_rpc:{r}"),
    }
}
