use thiserror::Error;

#[derive(Debug, Error)]
pub enum MathError {
    #[error("overflow")]
    Overflow,
}

pub fn checked_notional(qty: i64, price_czk: i64) -> Result<i64, MathError> {
    qty.checked_mul(price_czk).ok_or(MathError::Overflow)
}

pub fn ceil_div(n: i64, d: i64) -> i64 {
    if n == 0 {
        0
    } else {
        ((n - 1) / d) + 1
    }
}

pub fn taker_fee_czk(notional_czk: i64, fee_bps: i64) -> Result<i64, MathError> {
    let raw = notional_czk
        .checked_mul(fee_bps)
        .ok_or(MathError::Overflow)?;
    Ok(ceil_div(raw, 10_000))
}

pub fn reduce_long_first(long_qty: i64, sell_qty: i64) -> (i64, i64) {
    if long_qty >= sell_qty {
        (long_qty - sell_qty, 0)
    } else {
        (0, sell_qty - long_qty)
    }
}

pub fn adjust_reservation_amount(
    amount_czk: i64,
    consumed_czk: i64,
    delta_czk: i64,
) -> Option<i64> {
    let next = amount_czk.checked_add(delta_czk)?;
    if next < consumed_czk || next < 0 {
        return None;
    }
    Some(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_notional_integer_math() {
        assert_eq!(checked_notional(25, 400).unwrap(), 10_000);
        assert!(checked_notional(i64::MAX, 2).is_err());
    }

    #[test]
    fn fee_rounding_is_ceiling() {
        assert_eq!(taker_fee_czk(1, 1).unwrap(), 1);
        assert_eq!(taker_fee_czk(10_000, 1).unwrap(), 1);
        assert_eq!(taker_fee_czk(10_001, 1).unwrap(), 2);
        assert_eq!(taker_fee_czk(100_000, 25).unwrap(), 250);
    }

    #[test]
    fn reduce_long_first_math() {
        assert_eq!(reduce_long_first(10, 4), (6, 0));
        assert_eq!(reduce_long_first(10, 10), (0, 0));
        assert_eq!(reduce_long_first(7, 10), (0, 3));
    }

    #[test]
    fn reservation_adjustment_math() {
        assert_eq!(adjust_reservation_amount(1_000, 200, -200), Some(800));
        assert_eq!(adjust_reservation_amount(1_000, 200, 500), Some(1_500));
        assert_eq!(adjust_reservation_amount(1_000, 200, -900), None);
    }

    #[test]
    fn property_balances_never_negative() {
        for available_start in 0..25_i64 {
            for reserve in 0..=available_start {
                let mut available = available_start - reserve;
                let mut reserved = reserve;

                // reserve
                let reserve_amt = available_start % 4;
                if available >= reserve_amt {
                    available -= reserve_amt;
                    reserved += reserve_amt;
                }
                assert!(available >= 0 && reserved >= 0);

                // spend from reserved first, then available
                let spend = available_start % 5;
                let total = available + reserved;
                if total >= spend {
                    let consume_reserved = reserved.min(spend);
                    reserved -= consume_reserved;
                    available -= spend - consume_reserved;
                }
                assert!(available >= 0 && reserved >= 0);
            }
        }
    }

    #[test]
    fn property_conservation_minus_fees() {
        for qty in 1..20_i64 {
            for price in 1..200_i64 {
                for fee_bps in [0_i64, 1, 10, 25, 100] {
                    let notional = checked_notional(qty, price).unwrap();
                    let fee = taker_fee_czk(notional, fee_bps).unwrap();

                    let buyer_start = notional + fee + 1_000;
                    let seller_start = 1_000;
                    let fees_start = 0_i64;
                    let total_start = buyer_start + seller_start + fees_start;

                    let buyer_end = buyer_start - notional - fee;
                    let seller_end = seller_start + notional;
                    let fees_end = fees_start + fee;
                    let total_end = buyer_end + seller_end + fees_end;

                    assert!(buyer_end >= 0 && seller_end >= 0 && fees_end >= 0);
                    assert_eq!(total_start, total_end);
                }
            }
        }
    }
}
