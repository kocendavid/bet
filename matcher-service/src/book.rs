use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use blake3::Hasher;

use crate::types::{BookSnapshotView, Event, OpenOrderView, OrderType, PlaceOrder, Side};

#[derive(Debug, Clone)]
pub struct RestingOrder {
    pub order_id: String,
    pub user_id: String,
    pub side: Side,
    pub limit_price: i64,
    pub qty_remaining: i64,
    pub time_priority: u64,
}

#[derive(Debug, Clone)]
pub struct BookState {
    pub market_id: String,
    pub outcome_id: String,
    pub bids: BTreeMap<i64, VecDeque<String>>,
    pub asks: BTreeMap<i64, VecDeque<String>>,
    pub orders: HashMap<String, RestingOrder>,
    pub command_seq: u64,
    pub fill_seq: u64,
}

#[derive(Debug, Clone)]
pub struct ExecutedFill {
    pub fill_id: String,
    pub maker_order_id: String,
    pub maker_user_id: String,
    pub taker_order_id: String,
    pub price: i64,
    pub qty: i64,
}

#[derive(Debug, Clone)]
pub struct MatchResult {
    pub events: Vec<Event>,
    pub fills: Vec<ExecutedFill>,
    pub taker_qty_remaining: i64,
}

impl BookState {
    pub fn new(market_id: String, outcome_id: String) -> Self {
        Self {
            market_id,
            outcome_id,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            orders: HashMap::new(),
            command_seq: 0,
            fill_seq: 0,
        }
    }

    pub fn next_command_seq(&mut self) -> u64 {
        self.command_seq += 1;
        self.command_seq
    }

    pub fn reserve_buy_czk(limit_price: i64, qty: i64) -> Option<i64> {
        limit_price.checked_mul(qty)
    }

    pub fn reserve_sell_collateral_czk(limit_price: i64, qty: i64) -> Option<i64> {
        let per_share = (crate::types::FACE_CZK - limit_price).max(0);
        per_share.checked_mul(qty)
    }

    pub fn cancel_order(&mut self, user_id: &str, order_id: &str) -> Option<Event> {
        let order = self.orders.get(order_id)?;
        if order.user_id != user_id {
            return None;
        }

        let (side, price) = (order.side, order.limit_price);
        self.orders.remove(order_id);
        let levels = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };

        if let Some(queue) = levels.get_mut(&price) {
            if let Some(pos) = queue.iter().position(|id| id == order_id) {
                queue.remove(pos);
            }
            if queue.is_empty() {
                levels.remove(&price);
            }
        }

        Some(Event::OrderCanceled {
            order_id: order_id.to_string(),
        })
    }

    pub fn place_and_match(&mut self, cmd: &PlaceOrder) -> MatchResult {
        let mut events = vec![Event::OrderAccepted {
            order_id: cmd.order_id.clone(),
        }];
        let mut fills = Vec::new();

        let mut qty_remaining = cmd.qty;
        let mut skipped_self_levels: HashSet<i64> = HashSet::new();
        while qty_remaining > 0 {
            let best_cross_price =
                self.best_cross_price_excluding(cmd.side, cmd.limit_price, &skipped_self_levels);
            let Some(price) = best_cross_price else {
                break;
            };

            let queue_ids = self.peek_level_queue(cmd.side, price);
            let mut progressed = false;

            for maker_id in queue_ids {
                if qty_remaining <= 0 {
                    break;
                }

                let maker = match self.orders.get(&maker_id).cloned() {
                    Some(v) => v,
                    None => continue,
                };

                if maker.user_id == cmd.user_id {
                    // Self-trade prevention: skip maker and continue deterministic walk.
                    continue;
                }

                let trade_qty = qty_remaining.min(maker.qty_remaining);
                if trade_qty <= 0 {
                    continue;
                }

                qty_remaining -= trade_qty;
                let maker_remaining = maker.qty_remaining - trade_qty;
                progressed = true;

                self.fill_seq += 1;
                let fill_id = format!("fill-{}", self.fill_seq);
                events.push(Event::TradeExecuted {
                    fill_id: fill_id.clone(),
                    maker_order_id: maker.order_id.clone(),
                    taker_order_id: cmd.order_id.clone(),
                    price,
                    qty: trade_qty,
                });
                fills.push(ExecutedFill {
                    fill_id,
                    maker_order_id: maker.order_id.clone(),
                    maker_user_id: maker.user_id.clone(),
                    taker_order_id: cmd.order_id.clone(),
                    price,
                    qty: trade_qty,
                });

                if maker_remaining == 0 {
                    self.orders.remove(&maker.order_id);
                    self.remove_order_from_level(maker.side, maker.limit_price, &maker.order_id);
                    events.push(Event::OrderFilled {
                        order_id: maker.order_id,
                    });
                } else if let Some(entry) = self.orders.get_mut(&maker.order_id) {
                    entry.qty_remaining = maker_remaining;
                    events.push(Event::OrderPartial {
                        order_id: maker.order_id,
                        qty_remaining: maker_remaining,
                    });
                }
            }

            if !progressed {
                // All visible liquidity at this price level was self-owned.
                skipped_self_levels.insert(price);
                continue;
            }
        }

        if qty_remaining == 0 {
            events.push(Event::OrderFilled {
                order_id: cmd.order_id.clone(),
            });
            return MatchResult {
                events,
                fills,
                taker_qty_remaining: 0,
            };
        }

        match cmd.order_type {
            OrderType::Ioc => {
                events.push(Event::OrderCanceled {
                    order_id: cmd.order_id.clone(),
                });
            }
            OrderType::Limit => {
                let order = RestingOrder {
                    order_id: cmd.order_id.clone(),
                    user_id: cmd.user_id.clone(),
                    side: cmd.side,
                    limit_price: cmd.limit_price,
                    qty_remaining,
                    time_priority: self.command_seq,
                };
                self.orders.insert(cmd.order_id.clone(), order);
                self.push_order_to_level(cmd.side, cmd.limit_price, cmd.order_id.clone());
                events.push(Event::OrderRested {
                    order_id: cmd.order_id.clone(),
                    qty_remaining,
                });
            }
        }

        MatchResult {
            events,
            fills,
            taker_qty_remaining: qty_remaining,
        }
    }

    fn best_cross_price_excluding(
        &self,
        side: Side,
        limit_price: i64,
        skipped_levels: &HashSet<i64>,
    ) -> Option<i64> {
        match side {
            Side::Buy => self
                .asks
                .keys()
                .find(|ask| **ask <= limit_price && !skipped_levels.contains(ask))
                .copied(),
            Side::Sell => self
                .bids
                .keys()
                .rev()
                .find(|bid| **bid >= limit_price && !skipped_levels.contains(bid))
                .copied(),
        }
    }

    fn peek_level_queue(&self, taker_side: Side, price: i64) -> Vec<String> {
        let levels = match taker_side {
            Side::Buy => &self.asks,
            Side::Sell => &self.bids,
        };

        levels
            .get(&price)
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn remove_order_from_level(&mut self, side: Side, price: i64, order_id: &str) {
        let levels = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };

        if let Some(queue) = levels.get_mut(&price) {
            if let Some(pos) = queue.iter().position(|id| id == order_id) {
                queue.remove(pos);
            }
            if queue.is_empty() {
                levels.remove(&price);
            }
        }
    }

    fn push_order_to_level(&mut self, side: Side, price: i64, order_id: String) {
        let levels = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        levels.entry(price).or_default().push_back(order_id);
    }

    pub fn state_hash(&self) -> String {
        let mut hasher = Hasher::new();
        hasher.update(self.market_id.as_bytes());
        hasher.update(self.outcome_id.as_bytes());
        hasher.update(&self.command_seq.to_le_bytes());
        hasher.update(&self.fill_seq.to_le_bytes());

        let mut ids: Vec<_> = self.orders.keys().cloned().collect();
        ids.sort();
        for id in ids {
            if let Some(order) = self.orders.get(&id) {
                hasher.update(order.order_id.as_bytes());
                hasher.update(order.user_id.as_bytes());
                hasher.update(&order.limit_price.to_le_bytes());
                hasher.update(&order.qty_remaining.to_le_bytes());
                hasher.update(&order.time_priority.to_le_bytes());
                hasher.update(&[match order.side {
                    Side::Buy => 1,
                    Side::Sell => 2,
                }]);
            }
        }

        hasher.finalize().to_hex().to_string()
    }

    pub fn view(&self) -> BookSnapshotView {
        let bids = self
            .bids
            .iter()
            .rev()
            .map(|(price, q)| {
                let qty = q
                    .iter()
                    .filter_map(|id| self.orders.get(id))
                    .map(|o| o.qty_remaining)
                    .sum();
                (*price, qty)
            })
            .collect();

        let asks = self
            .asks
            .iter()
            .map(|(price, q)| {
                let qty = q
                    .iter()
                    .filter_map(|id| self.orders.get(id))
                    .map(|o| o.qty_remaining)
                    .sum();
                (*price, qty)
            })
            .collect();

        let mut open_orders: Vec<OpenOrderView> = self
            .orders
            .values()
            .map(|o| OpenOrderView {
                order_id: o.order_id.clone(),
                user_id: o.user_id.clone(),
                side: o.side,
                limit_price: o.limit_price,
                qty_remaining: o.qty_remaining,
                time_priority: o.time_priority,
            })
            .collect();
        open_orders.sort_by(|a, b| {
            (a.limit_price, a.time_priority, a.order_id.as_str()).cmp(&(
                b.limit_price,
                b.time_priority,
                b.order_id.as_str(),
            ))
        });

        BookSnapshotView {
            market_id: self.market_id.clone(),
            outcome_id: self.outcome_id.clone(),
            bids,
            asks,
            open_orders,
            state_hash: self.state_hash(),
            command_seq: self.command_seq,
            fill_seq: self.fill_seq,
        }
    }
}
