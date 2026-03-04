use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::sync::Mutex;

use crate::types::Event;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderStatus {
    Accepted,
    Open,
    Filled,
    Canceled,
    Rejected,
}

impl OrderStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            OrderStatus::Filled | OrderStatus::Canceled | OrderStatus::Rejected
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderLocation {
    pub user_id: String,
    pub order_id: String,
    pub client_order_id: String,
    pub market_id: String,
    pub outcome_id: String,
    pub shard_id: usize,
    pub current_status: OrderStatus,
    pub last_update_seq: u64,
    pub updated_at_epoch_secs: u64,
    pub expires_at_epoch_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedIndex {
    version: u32,
    next_seq: u64,
    by_order: HashMap<String, OrderLocation>,
    by_client: HashMap<String, String>,
}

impl Default for PersistedIndex {
    fn default() -> Self {
        Self {
            version: 1,
            next_seq: 1,
            by_order: HashMap::new(),
            by_client: HashMap::new(),
        }
    }
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn order_key(user_id: &str, order_id: &str) -> String {
    format!("{user_id}\n{order_id}")
}

fn client_key(user_id: &str, client_order_id: &str) -> String {
    format!("{user_id}\n{client_order_id}")
}

pub struct OrderIndex {
    path: PathBuf,
    terminal_ttl_secs: u64,
    state: Mutex<PersistedIndex>,
}

impl OrderIndex {
    pub async fn load(path: impl AsRef<Path>, terminal_ttl_secs: u64) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let state = match fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice::<PersistedIndex>(&bytes)
                .context("decode persisted order index")?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => PersistedIndex::default(),
            Err(err) => return Err(err.into()),
        };
        let index = Self {
            path,
            terminal_ttl_secs,
            state: Mutex::new(state),
        };
        index.compact_and_save().await?;
        Ok(index)
    }

    pub async fn lookup_by_order(&self, user_id: &str, order_id: &str) -> Option<OrderLocation> {
        self.lookup_by_order_key(&order_key(user_id, order_id))
            .await
    }

    pub async fn lookup_by_client(
        &self,
        user_id: &str,
        client_order_id: &str,
    ) -> Option<OrderLocation> {
        let key = client_key(user_id, client_order_id);
        let order_key = {
            let guard = self.state.lock().await;
            guard.by_client.get(&key).cloned()
        }?;
        self.lookup_by_order_key(&order_key).await
    }

    pub async fn lookup_by_order_or_client(
        &self,
        user_id: &str,
        id: &str,
    ) -> Option<OrderLocation> {
        if let Some(entry) = self.lookup_by_order(user_id, id).await {
            return Some(entry);
        }
        self.lookup_by_client(user_id, id).await
    }

    pub async fn claim_place_or_get_existing(
        &self,
        user_id: &str,
        order_id: &str,
        client_order_id: &str,
        market_id: &str,
        outcome_id: &str,
        shard_id: usize,
    ) -> anyhow::Result<(OrderLocation, bool)> {
        let mut guard = self.state.lock().await;
        compact_expired(&mut guard);

        let ck = client_key(user_id, client_order_id);
        if let Some(existing_order_key) = guard.by_client.get(&ck) {
            if let Some(existing) = guard.by_order.get(existing_order_key) {
                return Ok((existing.clone(), false));
            }
            guard.by_client.remove(&ck);
        }

        let now = now_epoch_secs();
        let seq = guard.next_seq;
        guard.next_seq += 1;
        let ok = order_key(user_id, order_id);
        let entry = OrderLocation {
            user_id: user_id.to_string(),
            order_id: order_id.to_string(),
            client_order_id: client_order_id.to_string(),
            market_id: market_id.to_string(),
            outcome_id: outcome_id.to_string(),
            shard_id,
            current_status: OrderStatus::Accepted,
            last_update_seq: seq,
            updated_at_epoch_secs: now,
            expires_at_epoch_secs: None,
        };
        guard.by_client.insert(ck, ok.clone());
        guard.by_order.insert(ok, entry.clone());
        drop(guard);
        self.save().await?;
        Ok((entry, true))
    }

    async fn lookup_by_order_key(&self, key: &str) -> Option<OrderLocation> {
        {
            let guard = self.state.lock().await;
            if let Some(entry) = guard.by_order.get(key) {
                if !is_expired(entry) {
                    return Some(entry.clone());
                }
            }
        }
        let _ = self.compact_and_save().await;
        let guard = self.state.lock().await;
        guard.by_order.get(key).cloned()
    }

    pub async fn upsert(
        &self,
        user_id: &str,
        order_id: &str,
        client_order_id: &str,
        market_id: &str,
        outcome_id: &str,
        shard_id: usize,
        status: OrderStatus,
    ) -> anyhow::Result<OrderLocation> {
        let mut guard = self.state.lock().await;
        let now = now_epoch_secs();
        let order_key = order_key(user_id, order_id);
        let client_key = client_key(user_id, client_order_id);
        let seq = guard.next_seq;
        guard.next_seq += 1;

        let expires = if status.is_terminal() {
            Some(now.saturating_add(self.terminal_ttl_secs))
        } else {
            None
        };

        let entry = OrderLocation {
            user_id: user_id.to_string(),
            order_id: order_id.to_string(),
            client_order_id: client_order_id.to_string(),
            market_id: market_id.to_string(),
            outcome_id: outcome_id.to_string(),
            shard_id,
            current_status: status,
            last_update_seq: seq,
            updated_at_epoch_secs: now,
            expires_at_epoch_secs: expires,
        };
        guard.by_client.insert(client_key, order_key.clone());
        guard.by_order.insert(order_key, entry.clone());
        drop(guard);
        self.save().await?;
        Ok(entry)
    }

    pub async fn apply_events(
        &self,
        user_id: &str,
        order_id: &str,
        client_order_id: &str,
        market_id: &str,
        outcome_id: &str,
        shard_id: usize,
        events: &[Event],
    ) -> anyhow::Result<OrderLocation> {
        let status = fold_status(events).unwrap_or(OrderStatus::Accepted);
        self.upsert(
            user_id,
            order_id,
            client_order_id,
            market_id,
            outcome_id,
            shard_id,
            status,
        )
        .await
    }

    async fn save(&self) -> anyhow::Result<()> {
        let bytes = {
            let guard = self.state.lock().await;
            serde_json::to_vec(&*guard).context("serialize order index")?
        };
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&self.path, bytes).await?;
        Ok(())
    }

    async fn compact_and_save(&self) -> anyhow::Result<()> {
        {
            let mut guard = self.state.lock().await;
            compact_expired(&mut guard);
        }
        self.save().await
    }
}

fn is_expired(entry: &OrderLocation) -> bool {
    match entry.expires_at_epoch_secs {
        Some(ts) => now_epoch_secs() > ts,
        None => false,
    }
}

fn compact_expired(state: &mut PersistedIndex) {
    let expired: Vec<String> = state
        .by_order
        .iter()
        .filter(|(_, v)| is_expired(v))
        .map(|(k, _)| k.clone())
        .collect();
    if expired.is_empty() {
        return;
    }
    for key in expired {
        if let Some(entry) = state.by_order.remove(&key) {
            let ck = client_key(&entry.user_id, &entry.client_order_id);
            state.by_client.remove(&ck);
        }
    }
}

fn fold_status(events: &[Event]) -> Option<OrderStatus> {
    let mut status = None;
    for event in events {
        let next = match event {
            Event::OrderAccepted { .. } => OrderStatus::Accepted,
            Event::OrderRested { .. } | Event::OrderPartial { .. } => OrderStatus::Open,
            Event::OrderFilled { .. } => OrderStatus::Filled,
            Event::OrderCanceled { .. } => OrderStatus::Canceled,
            Event::OrderRejected { .. } => OrderStatus::Rejected,
            Event::TradeExecuted { .. } => continue,
        };
        status = Some(next);
    }
    status
}
