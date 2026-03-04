use std::collections::HashSet;
use std::sync::Arc;

use matcher_service::ledger::{FillIntent, LedgerAdapter, LedgerError, ReservationKindLocal};
use matcher_service::persistence::Storage;
use matcher_service::service::MatcherService;
use matcher_service::types::{Command, OrderType, PlaceOrder, Side};
use tempfile::tempdir;
use tokio::sync::Mutex;

#[derive(Default, Clone)]
struct MockLedger {
    fail_reserve_orders: Arc<Mutex<HashSet<String>>>,
    releases: Arc<Mutex<Vec<String>>>,
    fills: Arc<Mutex<Vec<String>>>,
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
        order_id: &str,
    ) -> Result<(), LedgerError> {
        self.releases.lock().await.push(order_id.to_string());
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

    async fn apply_fill(&self, intent: FillIntent) -> Result<(), LedgerError> {
        self.fills.lock().await.push(intent.fill_id);
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn place(
    cmd_id: &str,
    market_id: &str,
    outcome_id: &str,
    user: &str,
    order: &str,
    side: Side,
    typ: OrderType,
    price: i64,
    qty: i64,
) -> Command {
    Command::Place(PlaceOrder {
        command_id: cmd_id.into(),
        market_id: market_id.into(),
        outcome_id: outcome_id.into(),
        user_id: user.into(),
        order_id: order.into(),
        side,
        order_type: typ,
        limit_price: price,
        qty,
    })
}

#[tokio::test]
async fn replay_determinism_state_hash_stable() {
    let dir = tempdir().unwrap();
    let storage = Storage::new(dir.path());
    let ledger = MockLedger::default();

    let mut svc = MatcherService::new("m1".into(), "o1".into(), storage.clone(), ledger.clone(), 0)
        .await
        .unwrap();

    let cmds = vec![
        place(
            "c1",
            "m1",
            "o1",
            "u1",
            "o1",
            Side::Sell,
            OrderType::Limit,
            6000,
            5,
        ),
        place(
            "c2",
            "m1",
            "o1",
            "u2",
            "o2",
            Side::Sell,
            OrderType::Limit,
            6200,
            4,
        ),
        place(
            "c3",
            "m1",
            "o1",
            "u3",
            "o3",
            Side::Buy,
            OrderType::Limit,
            6200,
            6,
        ),
        place(
            "c4",
            "m1",
            "o1",
            "u4",
            "o4",
            Side::Buy,
            OrderType::Ioc,
            6100,
            5,
        ),
    ];

    for c in cmds {
        let _ = svc.process(c).await.unwrap();
    }
    let hash_a = svc.book.state_hash();

    let svc2 = MatcherService::new("m1".into(), "o1".into(), storage.clone(), ledger.clone(), 0)
        .await
        .unwrap();

    let hash_b = svc2.book.state_hash();
    assert_eq!(hash_a, hash_b);
}

#[tokio::test]
async fn multi_level_crossing_and_partial_rest() {
    let dir = tempdir().unwrap();
    let storage = Storage::new(dir.path());
    let ledger = MockLedger::default();
    let mut svc = MatcherService::new("m".into(), "o".into(), storage, ledger, 0)
        .await
        .unwrap();

    svc.process(place(
        "c1",
        "m",
        "o",
        "s1",
        "ask1",
        Side::Sell,
        OrderType::Limit,
        5000,
        4,
    ))
    .await
    .unwrap();
    svc.process(place(
        "c2",
        "m",
        "o",
        "s2",
        "ask2",
        Side::Sell,
        OrderType::Limit,
        5100,
        3,
    ))
    .await
    .unwrap();

    let out = svc
        .process(place(
            "c3",
            "m",
            "o",
            "b1",
            "bid1",
            Side::Buy,
            OrderType::Limit,
            5200,
            10,
        ))
        .await
        .unwrap();

    let trades = out
        .events
        .iter()
        .filter(|e| matches!(e, matcher_service::types::Event::TradeExecuted { .. }))
        .count();
    assert_eq!(trades, 2);

    let bid = svc.book.orders.get("bid1").unwrap();
    assert_eq!(bid.qty_remaining, 3);
    assert_eq!(bid.limit_price, 5200);
}

#[tokio::test]
async fn ioc_remainder_canceled_not_rested() {
    let dir = tempdir().unwrap();
    let storage = Storage::new(dir.path());
    let ledger = MockLedger::default();
    let mut svc = MatcherService::new("m".into(), "o".into(), storage, ledger.clone(), 0)
        .await
        .unwrap();

    svc.process(place(
        "c1",
        "m",
        "o",
        "s1",
        "ask1",
        Side::Sell,
        OrderType::Limit,
        5000,
        2,
    ))
    .await
    .unwrap();
    let out = svc
        .process(place(
            "c2",
            "m",
            "o",
            "b1",
            "ioc1",
            Side::Buy,
            OrderType::Ioc,
            5000,
            5,
        ))
        .await
        .unwrap();

    assert!(out
        .events
        .iter()
        .any(|e| matches!(e, matcher_service::types::Event::OrderCanceled { order_id } if order_id == "ioc1")));
    assert!(!svc.book.orders.contains_key("ioc1"));

    let releases = ledger.releases.lock().await.clone();
    assert!(releases.iter().any(|id| id == "ioc1"));
}

#[tokio::test]
async fn self_trade_skip_deterministic() {
    let dir = tempdir().unwrap();
    let storage = Storage::new(dir.path());
    let ledger = MockLedger::default();
    let mut svc = MatcherService::new("m".into(), "o".into(), storage, ledger, 0)
        .await
        .unwrap();

    svc.process(place(
        "c1",
        "m",
        "o",
        "u1",
        "ask_self",
        Side::Sell,
        OrderType::Limit,
        4900,
        3,
    ))
    .await
    .unwrap();
    svc.process(place(
        "c2",
        "m",
        "o",
        "u2",
        "ask_other",
        Side::Sell,
        OrderType::Limit,
        5000,
        2,
    ))
    .await
    .unwrap();

    let out = svc
        .process(place(
            "c3",
            "m",
            "o",
            "u1",
            "buy1",
            Side::Buy,
            OrderType::Limit,
            5000,
            4,
        ))
        .await
        .unwrap();

    let trades = out
        .events
        .iter()
        .filter(|e| matches!(e, matcher_service::types::Event::TradeExecuted { .. }))
        .count();
    assert_eq!(trades, 1);

    assert!(svc.book.orders.contains_key("ask_self"));
}

#[tokio::test]
async fn reservation_reject_does_not_mutate_book() {
    let dir = tempdir().unwrap();
    let storage = Storage::new(dir.path());
    let ledger = MockLedger::default();
    ledger
        .fail_reserve_orders
        .lock()
        .await
        .insert("bad-order".into());

    let mut svc = MatcherService::new("m".into(), "o".into(), storage, ledger, 0)
        .await
        .unwrap();

    let out = svc
        .process(place(
            "c1",
            "m",
            "o",
            "u1",
            "bad-order",
            Side::Buy,
            OrderType::Limit,
            5100,
            5,
        ))
        .await
        .unwrap();

    assert!(out
        .events
        .iter()
        .any(|e| matches!(e, matcher_service::types::Event::OrderRejected { .. })));
    assert!(svc.book.orders.is_empty());
}
