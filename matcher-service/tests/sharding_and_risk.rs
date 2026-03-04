use std::collections::HashSet;
use std::sync::Arc;

use matcher_service::ledger::{FillIntent, LedgerAdapter, LedgerError, ReservationKindLocal};
use matcher_service::risk::RiskLimits;
use matcher_service::sharding::{stable_shard_for_market, ShardRuntime};
use matcher_service::types::{Command, Event, OrderType, PlaceOrder, Side};
use tempfile::tempdir;
use tokio::sync::Mutex;

#[derive(Default, Clone)]
struct MockLedger {
    fail_reserve_orders: Arc<Mutex<HashSet<String>>>,
}

#[tonic::async_trait]
impl LedgerAdapter for MockLedger {
    async fn reserve_for_order(
        &self,
        _command_id: &str,
        _user_id: &str,
        order_id: &str,
        _kind: ReservationKindLocal,
        _amount_czk: i64,
    ) -> Result<(), LedgerError> {
        if self.fail_reserve_orders.lock().await.contains(order_id) {
            return Err(LedgerError::Rejected("insufficient".into()));
        }
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
}

#[allow(clippy::too_many_arguments)]
fn place(
    cmd_id: &str,
    market_id: &str,
    outcome_id: &str,
    user_id: &str,
    order_id: &str,
    side: Side,
    order_type: OrderType,
    limit_price: i64,
    qty: i64,
) -> Command {
    Command::Place(PlaceOrder {
        command_id: cmd_id.to_string(),
        market_id: market_id.to_string(),
        outcome_id: outcome_id.to_string(),
        user_id: user_id.to_string(),
        order_id: order_id.to_string(),
        side,
        order_type,
        limit_price,
        qty,
    })
}

#[tokio::test]
async fn routing_stability_same_input_same_shard() {
    let shard_count = 2;
    let ids = ["market-a", "market-b", "market-c", "100", "101", "102"];
    for market_id in ids {
        let baseline = stable_shard_for_market(market_id, shard_count);
        for _ in 0..100 {
            assert_eq!(baseline, stable_shard_for_market(market_id, shard_count));
        }
    }
}

#[tokio::test]
async fn randomized_multi_market_is_deterministic() {
    let dir1 = tempdir().unwrap();
    let dir2 = tempdir().unwrap();
    let ledger = MockLedger::default();
    let runtime1 =
        ShardRuntime::new(2, dir1.path(), ledger.clone(), 0, RiskLimits::default()).unwrap();
    let runtime2 =
        ShardRuntime::new(2, dir2.path(), ledger.clone(), 0, RiskLimits::default()).unwrap();

    let mut seed: u64 = 7;
    let markets = ["m1", "m2", "m3", "m4"];
    let users = ["u1", "u2", "u3"];
    for i in 0..200 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let market = markets[(seed as usize) % markets.len()];
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let user = users[(seed as usize) % users.len()];
        let side = if (seed & 1) == 0 {
            Side::Buy
        } else {
            Side::Sell
        };
        let price = 4_000 + ((seed % 2_000) as i64);
        let qty = 1 + ((seed % 5) as i64);
        let order_id = format!("o-{i}");
        let cmd = place(
            &format!("c-{i}"),
            market,
            "yes",
            user,
            &order_id,
            side,
            OrderType::Limit,
            price,
            qty,
        );
        let _ = runtime1.process(cmd.clone()).await.unwrap();
        let _ = runtime2.process(cmd).await.unwrap();
    }

    for market in markets {
        let a = runtime1
            .get_book(market.to_string(), "yes".to_string())
            .await
            .unwrap();
        let b = runtime2
            .get_book(market.to_string(), "yes".to_string())
            .await
            .unwrap();
        match (a, b) {
            (Some(left), Some(right)) => assert_eq!(left["state_hash"], right["state_hash"]),
            (None, None) => {}
            _ => panic!("book presence diverged across deterministic runs"),
        }
    }
}

#[tokio::test]
async fn migration_replays_and_preserves_hash() {
    let dir = tempdir().unwrap();
    let ledger = MockLedger::default();
    let runtime = ShardRuntime::new(2, dir.path(), ledger, 0, RiskLimits::default()).unwrap();

    for i in 0..20 {
        let cmd = place(
            &format!("c-{i}"),
            "hot-market",
            "yes",
            if i % 2 == 0 { "maker" } else { "taker" },
            &format!("o-{i}"),
            if i % 2 == 0 { Side::Sell } else { Side::Buy },
            OrderType::Limit,
            5_000,
            2,
        );
        let _ = runtime.process(cmd).await.unwrap();
    }

    let before = runtime
        .get_book("hot-market".to_string(), "yes".to_string())
        .await
        .unwrap()
        .unwrap();
    let source = runtime.route_with_overrides("hot-market").await;
    let target = if source == 0 { 1 } else { 0 };
    runtime
        .migrate_market("hot-market".to_string(), "yes".to_string(), target)
        .await
        .unwrap();

    let after = runtime
        .get_book("hot-market".to_string(), "yes".to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before["state_hash"], after["state_hash"]);
    assert_eq!(runtime.route_with_overrides("hot-market").await, target);
}

#[tokio::test]
async fn risk_limit_rejections_cover_all_paths() {
    let dir = tempdir().unwrap();
    let limits = RiskLimits {
        max_open_orders: 1,
        max_qty_per_order: 5,
        max_notional_per_order_czk: 20_000,
        max_short_exposure_czk: 4_000,
        min_tick: 1000,
        max_tick: 6000,
    };
    let runtime = ShardRuntime::new(2, dir.path(), MockLedger::default(), 0, limits).unwrap();

    let over_qty = runtime
        .process(place(
            "q1",
            "risk-market",
            "yes",
            "u1",
            "oq",
            Side::Buy,
            OrderType::Limit,
            2000,
            6,
        ))
        .await
        .unwrap();
    assert!(has_reason(&over_qty.events, "risk_max_qty_per_order"));

    let over_notional = runtime
        .process(place(
            "q2",
            "risk-market",
            "yes",
            "u1",
            "on",
            Side::Buy,
            OrderType::Limit,
            5000,
            5,
        ))
        .await
        .unwrap();
    assert!(has_reason(
        &over_notional.events,
        "risk_max_notional_per_order"
    ));

    let over_price_band = runtime
        .process(place(
            "q3",
            "risk-market",
            "yes",
            "u1",
            "pb",
            Side::Buy,
            OrderType::Limit,
            8000,
            1,
        ))
        .await
        .unwrap();
    assert!(has_reason(
        &over_price_band.events,
        "risk_price_band_violation"
    ));

    let valid = runtime
        .process(place(
            "q4",
            "risk-market",
            "yes",
            "u1",
            "ok",
            Side::Buy,
            OrderType::Limit,
            2000,
            2,
        ))
        .await
        .unwrap();
    assert!(!has_any_reject(&valid.events));

    let open_order_limit = runtime
        .process(place(
            "q5",
            "risk-market",
            "yes",
            "u1",
            "o2",
            Side::Buy,
            OrderType::Limit,
            2000,
            1,
        ))
        .await
        .unwrap();
    assert!(has_reason(&open_order_limit.events, "risk_max_open_orders"));

    let short_exposure = runtime
        .process(place(
            "q6",
            "risk-market",
            "yes",
            "seller",
            "s1",
            Side::Sell,
            OrderType::Limit,
            1000,
            1,
        ))
        .await
        .unwrap();
    assert!(has_reason(
        &short_exposure.events,
        "risk_max_short_exposure"
    ));
}

fn has_reason(events: &[Event], reason: &str) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            Event::OrderRejected {
                reason: reject_reason,
                ..
            } if reject_reason == reason
        )
    })
}

fn has_any_reject(events: &[Event]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, Event::OrderRejected { .. }))
}
