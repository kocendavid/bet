use crate::book::BookState;
use crate::types::{PlaceOrder, Side, FACE_CZK};

#[derive(Debug, Clone)]
pub struct RiskLimits {
    pub max_open_orders: usize,
    pub max_qty_per_order: i64,
    pub max_notional_per_order_czk: i64,
    pub max_short_exposure_czk: i64,
    pub min_tick: i64,
    pub max_tick: i64,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_open_orders: 100,
            max_qty_per_order: 1_000_000,
            max_notional_per_order_czk: 10_000_000_000,
            max_short_exposure_czk: 10_000_000_000,
            min_tick: 0,
            max_tick: FACE_CZK,
        }
    }
}

pub fn check_place_risk(
    book: &BookState,
    cmd: &PlaceOrder,
    limits: &RiskLimits,
) -> Result<(), String> {
    if cmd.limit_price < limits.min_tick || cmd.limit_price > limits.max_tick {
        return Err("risk_price_band_violation".into());
    }

    if cmd.qty > limits.max_qty_per_order {
        return Err("risk_max_qty_per_order".into());
    }

    let notional = cmd
        .limit_price
        .checked_mul(cmd.qty)
        .ok_or_else(|| "risk_notional_overflow".to_string())?;
    if notional > limits.max_notional_per_order_czk {
        return Err("risk_max_notional_per_order".into());
    }

    let current_open_orders = book
        .orders
        .values()
        .filter(|order| order.user_id == cmd.user_id)
        .count();
    if current_open_orders + 1 > limits.max_open_orders {
        return Err("risk_max_open_orders".into());
    }

    if cmd.side == Side::Sell {
        let current_short = book
            .orders
            .values()
            .filter(|order| order.user_id == cmd.user_id && order.side == Side::Sell)
            .try_fold(0_i64, |acc, order| {
                let per_share = (FACE_CZK - order.limit_price).max(0);
                per_share
                    .checked_mul(order.qty_remaining)
                    .and_then(|v| acc.checked_add(v))
            })
            .ok_or_else(|| "risk_short_exposure_overflow".to_string())?;
        let incoming_per_share = (FACE_CZK - cmd.limit_price).max(0);
        let incoming_short = incoming_per_share
            .checked_mul(cmd.qty)
            .ok_or_else(|| "risk_short_exposure_overflow".to_string())?;
        let total_short = current_short
            .checked_add(incoming_short)
            .ok_or_else(|| "risk_short_exposure_overflow".to_string())?;
        if total_short > limits.max_short_exposure_czk {
            return Err("risk_max_short_exposure".into());
        }
    }

    Ok(())
}
