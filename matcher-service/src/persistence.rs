use std::io::{ErrorKind, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use prost::Message;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::book::{BookState, RestingOrder};
use crate::types::{CancelOrder, Command, OrderType, PlaceOrder, Side};

#[derive(Clone, PartialEq, Message)]
struct LogRecord {
    #[prost(uint64, tag = "1")]
    seq: u64,
    #[prost(message, optional, tag = "2")]
    command: Option<PbCommand>,
}

#[derive(Clone, PartialEq, Message)]
struct PbCommand {
    #[prost(string, tag = "1")]
    command_id: String,
    #[prost(oneof = "pb_command::Kind", tags = "2, 3")]
    kind: Option<pb_command::Kind>,
}

mod pb_command {
    use prost::Oneof;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Kind {
        #[prost(message, tag = "2")]
        Place(super::PbPlace),
        #[prost(message, tag = "3")]
        Cancel(super::PbCancel),
    }
}

#[derive(Clone, PartialEq, Message)]
struct PbPlace {
    #[prost(string, tag = "1")]
    user_id: String,
    #[prost(string, tag = "2")]
    order_id: String,
    #[prost(int32, tag = "3")]
    side: i32,
    #[prost(int32, tag = "4")]
    order_type: i32,
    #[prost(int64, tag = "5")]
    limit_price: i64,
    #[prost(int64, tag = "6")]
    qty: i64,
}

#[derive(Clone, PartialEq, Message)]
struct PbCancel {
    #[prost(string, tag = "1")]
    user_id: String,
    #[prost(string, tag = "2")]
    order_id: String,
}

#[derive(Clone, PartialEq, Message)]
struct PbSnapshot {
    #[prost(string, tag = "1")]
    market_id: String,
    #[prost(string, tag = "2")]
    outcome_id: String,
    #[prost(uint64, tag = "3")]
    command_seq: u64,
    #[prost(uint64, tag = "4")]
    fill_seq: u64,
    #[prost(message, repeated, tag = "5")]
    orders: Vec<PbOrder>,
    #[prost(string, repeated, tag = "6")]
    command_ids: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
struct PbOrder {
    #[prost(string, tag = "1")]
    order_id: String,
    #[prost(string, tag = "2")]
    user_id: String,
    #[prost(int32, tag = "3")]
    side: i32,
    #[prost(int64, tag = "4")]
    limit_price: i64,
    #[prost(int64, tag = "5")]
    qty_remaining: i64,
    #[prost(uint64, tag = "6")]
    time_priority: u64,
}

#[derive(Clone)]
pub struct Storage {
    log_path: PathBuf,
    snapshot_path: PathBuf,
}

pub struct LoadedState {
    pub book: BookState,
    pub processed_command_ids: Vec<String>,
    pub replay_commands: Vec<Command>,
}

impl Storage {
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        let base = base_dir.as_ref();
        Self {
            log_path: base.join("commands.log"),
            snapshot_path: base.join("snapshot.pb"),
        }
    }

    pub async fn append_command(&self, seq: u64, command: &Command) -> anyhow::Result<()> {
        if let Some(parent) = self.log_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let record = LogRecord {
            seq,
            command: Some(command_to_pb(command)),
        };

        let mut buf = Vec::new();
        record.encode(&mut buf)?;

        let len = (buf.len() as u32).to_le_bytes();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .await
            .context("open command log")?;

        file.write_all(&len).await?;
        file.write_all(&buf).await?;
        file.flush().await?;
        Ok(())
    }

    pub async fn write_snapshot(
        &self,
        book: &BookState,
        processed_command_ids: &[String],
    ) -> anyhow::Result<()> {
        if let Some(parent) = self.snapshot_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let mut orders: Vec<_> = book.orders.values().cloned().collect();
        orders.sort_by(|a, b| {
            (a.time_priority, a.order_id.as_str()).cmp(&(b.time_priority, b.order_id.as_str()))
        });

        let snapshot = PbSnapshot {
            market_id: book.market_id.clone(),
            outcome_id: book.outcome_id.clone(),
            command_seq: book.command_seq,
            fill_seq: book.fill_seq,
            orders: orders
                .into_iter()
                .map(|o| PbOrder {
                    order_id: o.order_id,
                    user_id: o.user_id,
                    side: side_to_i32(o.side),
                    limit_price: o.limit_price,
                    qty_remaining: o.qty_remaining,
                    time_priority: o.time_priority,
                })
                .collect(),
            command_ids: processed_command_ids.to_vec(),
        };

        let mut buf = Vec::new();
        snapshot.encode(&mut buf)?;
        fs::write(&self.snapshot_path, buf).await?;
        Ok(())
    }

    pub async fn load(
        &self,
        market_id: String,
        outcome_id: String,
    ) -> anyhow::Result<LoadedState> {
        let mut book = BookState::new(market_id, outcome_id);
        let mut processed = Vec::new();

        if let Ok(bytes) = fs::read(&self.snapshot_path).await {
            let snap = PbSnapshot::decode(bytes.as_slice())?;
            book.market_id = snap.market_id;
            book.outcome_id = snap.outcome_id;
            book.command_seq = snap.command_seq;
            book.fill_seq = snap.fill_seq;
            processed = snap.command_ids;

            for o in snap.orders {
                let order = RestingOrder {
                    order_id: o.order_id,
                    user_id: o.user_id,
                    side: side_from_i32(o.side)?,
                    limit_price: o.limit_price,
                    qty_remaining: o.qty_remaining,
                    time_priority: o.time_priority,
                };
                book.orders.insert(order.order_id.clone(), order.clone());
                match order.side {
                    Side::Buy => book
                        .bids
                        .entry(order.limit_price)
                        .or_default()
                        .push_back(order.order_id),
                    Side::Sell => book
                        .asks
                        .entry(order.limit_price)
                        .or_default()
                        .push_back(order.order_id),
                }
            }
        }

        let replay = self.read_log_suffix(book.command_seq).await?;
        Ok(LoadedState {
            book,
            processed_command_ids: processed,
            replay_commands: replay,
        })
    }

    async fn read_log_suffix(&self, min_seq_exclusive: u64) -> anyhow::Result<Vec<Command>> {
        let mut file = match File::open(&self.log_path).await {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };

        file.seek(SeekFrom::Start(0)).await?;
        let mut commands = Vec::new();

        loop {
            let mut len_bytes = [0_u8; 4];
            match file.read_exact(&mut len_bytes).await {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
            let len = u32::from_le_bytes(len_bytes) as usize;
            let mut buf = vec![0_u8; len];
            file.read_exact(&mut buf).await?;
            let rec = LogRecord::decode(buf.as_slice())?;
            if rec.seq <= min_seq_exclusive {
                continue;
            }
            let cmd = pb_to_command(rec.command.ok_or_else(|| anyhow!("missing command"))?)?;
            commands.push(cmd);
        }

        Ok(commands)
    }
}

fn command_to_pb(command: &Command) -> PbCommand {
    match command {
        Command::Place(c) => PbCommand {
            command_id: c.command_id.clone(),
            kind: Some(pb_command::Kind::Place(PbPlace {
                user_id: c.user_id.clone(),
                order_id: c.order_id.clone(),
                side: side_to_i32(c.side),
                order_type: order_type_to_i32(c.order_type),
                limit_price: c.limit_price,
                qty: c.qty,
            })),
        },
        Command::Cancel(c) => PbCommand {
            command_id: c.command_id.clone(),
            kind: Some(pb_command::Kind::Cancel(PbCancel {
                user_id: c.user_id.clone(),
                order_id: c.order_id.clone(),
            })),
        },
    }
}

fn pb_to_command(pb: PbCommand) -> anyhow::Result<Command> {
    let kind = pb.kind.ok_or_else(|| anyhow!("missing command kind"))?;
    Ok(match kind {
        pb_command::Kind::Place(p) => Command::Place(PlaceOrder {
            command_id: pb.command_id,
            user_id: p.user_id,
            order_id: p.order_id,
            side: side_from_i32(p.side)?,
            order_type: order_type_from_i32(p.order_type)?,
            limit_price: p.limit_price,
            qty: p.qty,
        }),
        pb_command::Kind::Cancel(c) => Command::Cancel(CancelOrder {
            command_id: pb.command_id,
            user_id: c.user_id,
            order_id: c.order_id,
        }),
    })
}

fn side_to_i32(s: Side) -> i32 {
    match s {
        Side::Buy => 1,
        Side::Sell => 2,
    }
}

fn side_from_i32(v: i32) -> anyhow::Result<Side> {
    match v {
        1 => Ok(Side::Buy),
        2 => Ok(Side::Sell),
        _ => Err(anyhow!("invalid side")),
    }
}

fn order_type_to_i32(t: OrderType) -> i32 {
    match t {
        OrderType::Limit => 1,
        OrderType::Ioc => 2,
    }
}

fn order_type_from_i32(v: i32) -> anyhow::Result<OrderType> {
    match v {
        1 => Ok(OrderType::Limit),
        2 => Ok(OrderType::Ioc),
        _ => Err(anyhow!("invalid order_type")),
    }
}
