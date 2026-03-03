use ledger_service::pb::ledger_service_server::LedgerService;
use ledger_service::pb::{
    ApplyFillRequest, GetBalancesRequest, GetPositionsRequest, Party, ReservationKind,
    ReserveForOrderRequest, Side,
};
use ledger_service::service::LedgerGrpcService;
use serial_test::serial;
use sqlx::{postgres::PgPoolOptions, PgPool, Row};
use tonic::Request;

async fn setup_db() -> PgPool {
    let database_url = std::env::var("TEST_DATABASE_URL")
        .expect("TEST_DATABASE_URL must be set for integration tests");

    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect(&database_url)
        .await
        .expect("connect db");

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("run migrations");

    sqlx::query("TRUNCATE fills, reservations, positions, processed_commands, wallets")
        .execute(&pool)
        .await
        .expect("truncate tables");

    pool
}

#[tokio::test]
#[serial]
#[ignore = "requires TEST_DATABASE_URL"]
async fn apply_fill_duplicate_fill_id_is_idempotent_noop() {
    let pool = setup_db().await;
    let svc = LedgerGrpcService::new(pool.clone());

    sqlx::query("INSERT INTO wallets(user_id, available_czk, reserved_czk) VALUES('maker', 0, 0),('taker', 20000, 0)")
        .execute(&pool)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO positions(user_id, market_id, outcome_id, long_qty, short_qty) VALUES('maker','m1','o1',10,0)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let reserve = svc
        .reserve_for_order(Request::new(ReserveForOrderRequest {
            command_id: "cmd-reserve-1".into(),
            reservation_id: "resv-taker-1".into(),
            user_id: "taker".into(),
            order_id: "order-taker-1".into(),
            kind: ReservationKind::BuyReserve as i32,
            amount_czk: 505,
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(reserve.ok);

    let req = ApplyFillRequest {
        fill_id: "fill-1".into(),
        maker: Some(Party {
            user_id: "maker".into(),
            reservation_id: "".into(),
            order_id: "order-maker-1".into(),
        }),
        taker: Some(Party {
            user_id: "taker".into(),
            reservation_id: "resv-taker-1".into(),
            order_id: "order-taker-1".into(),
        }),
        market_id: "m1".into(),
        outcome_id: "o1".into(),
        qty: 5,
        price_czk: 100,
        taker_side: Side::Buy as i32,
        fee_bps: 100,
    };

    let first = svc
        .apply_fill(Request::new(req.clone()))
        .await
        .unwrap()
        .into_inner();
    assert!(first.ok);
    assert_eq!(first.notional_czk, 500);
    assert_eq!(first.fee_czk, 5);

    let second = svc
        .apply_fill(Request::new(req))
        .await
        .unwrap()
        .into_inner();
    assert!(second.ok);
    assert_eq!(second.reason, "idempotent replay");

    let taker_balance = svc
        .get_balances(Request::new(GetBalancesRequest {
            user_id: "taker".into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(taker_balance.available_czk, 19_495);
    assert_eq!(taker_balance.reserved_czk, 0);

    let maker_balance = svc
        .get_balances(Request::new(GetBalancesRequest {
            user_id: "maker".into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(maker_balance.available_czk, 500);
    assert_eq!(maker_balance.reserved_czk, 0);

    let fill_count: i64 = sqlx::query("SELECT COUNT(*) FROM fills WHERE fill_id='fill-1'")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get(0);
    assert_eq!(fill_count, 1);

    let maker_positions = svc
        .get_positions(Request::new(GetPositionsRequest {
            user_id: "maker".into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(maker_positions.positions[0].long_qty, 5);
    assert_eq!(maker_positions.positions[0].short_qty, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[serial]
#[ignore = "requires TEST_DATABASE_URL"]
async fn concurrency_parallel_reserve_requests_preserve_invariants() {
    let pool = setup_db().await;
    let svc = LedgerGrpcService::new(pool.clone());

    sqlx::query("INSERT INTO wallets(user_id, available_czk, reserved_czk) VALUES('u1', 1000, 0)")
        .execute(&pool)
        .await
        .unwrap();

    let mut handles = Vec::new();
    for i in 0..120_i64 {
        let svc_clone = svc.clone();
        handles.push(tokio::spawn(async move {
            svc_clone
                .reserve_for_order(Request::new(ReserveForOrderRequest {
                    command_id: format!("cmd-{i}"),
                    reservation_id: format!("resv-{i}"),
                    user_id: "u1".into(),
                    order_id: format!("order-{i}"),
                    kind: ReservationKind::BuyReserve as i32,
                    amount_czk: 10,
                }))
                .await
        }));
    }

    let mut ok_count = 0_i64;
    for h in handles {
        if let Ok(Ok(resp)) = h.await {
            if resp.into_inner().ok {
                ok_count += 1;
            }
        }
    }

    let row = sqlx::query("SELECT available_czk, reserved_czk FROM wallets WHERE user_id='u1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let available: i64 = row.get(0);
    let reserved: i64 = row.get(1);

    assert!(available >= 0);
    assert!(reserved >= 0);
    assert_eq!(available + reserved, 1000);
    assert!(ok_count <= 100);
}
