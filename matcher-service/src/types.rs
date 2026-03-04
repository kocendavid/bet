use serde::{Deserialize, Serialize};

pub const FACE_CZK: i64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    Limit,
    Ioc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaceOrder {
    pub command_id: String,
    pub market_id: String,
    pub outcome_id: String,
    pub user_id: String,
    pub order_id: String,
    pub side: Side,
    pub order_type: OrderType,
    pub limit_price: i64,
    pub qty: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelOrder {
    pub command_id: String,
    pub market_id: String,
    pub outcome_id: String,
    pub user_id: String,
    pub order_id: String,
}

#[derive(Debug, Clone)]
pub enum Command {
    Place(PlaceOrder),
    Cancel(CancelOrder),
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    OrderAccepted {
        order_id: String,
    },
    OrderRejected {
        order_id: String,
        reason: String,
    },
    OrderRested {
        order_id: String,
        qty_remaining: i64,
    },
    OrderCanceled {
        order_id: String,
    },
    TradeExecuted {
        fill_id: String,
        maker_order_id: String,
        taker_order_id: String,
        price: i64,
        qty: i64,
    },
    OrderPartial {
        order_id: String,
        qty_remaining: i64,
    },
    OrderFilled {
        order_id: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenOrderView {
    pub order_id: String,
    pub user_id: String,
    pub side: Side,
    pub limit_price: i64,
    pub qty_remaining: i64,
    pub time_priority: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BookSnapshotView {
    pub market_id: String,
    pub outcome_id: String,
    pub bids: Vec<(i64, i64)>,
    pub asks: Vec<(i64, i64)>,
    pub open_orders: Vec<OpenOrderView>,
    pub state_hash: String,
    pub command_seq: u64,
    pub fill_seq: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommandResult {
    pub command_id: String,
    pub events: Vec<Event>,
}
