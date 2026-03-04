use matcher_service::streaming::{
    AuthClaims, ChannelKind, StaticTokenAuthorizer, StreamEnvelope, StreamHub, StreamPayload,
    SubscriptionRequest, WsAuthorizer,
};
use matcher_service::types::{Command, CommandResult, Event, OrderType, PlaceOrder, Side};

#[tokio::test]
async fn user_channel_delivers_ordered_lossless_critical_updates() {
    let hub = StreamHub::new();
    let auth = StaticTokenAuthorizer::with_tokens([(
        "user-token".to_string(),
        AuthClaims {
            user_id: Some("u1".to_string()),
            can_market_stream: true,
        },
    )]);

    let claims = auth.authorize("user-token").await.unwrap();
    let mut subscription = hub
        .subscribe(
            &claims,
            SubscriptionRequest {
                channel: ChannelKind::User,
                market_id: None,
                user_id: Some("u1".to_string()),
                include_depth_snapshots: false,
            },
            16,
        )
        .await
        .unwrap();

    let cmd = Command::Place(PlaceOrder {
        command_id: "cmd-1".into(),
        market_id: "m1".into(),
        outcome_id: "yes".into(),
        user_id: "u1".into(),
        client_order_id: "00000000-0000-4000-8000-000000000001".into(),
        order_id: "o1".into(),
        side: Side::Buy,
        order_type: OrderType::Limit,
        limit_price: 5000,
        qty: 2,
    });

    let result = CommandResult {
        command_id: "cmd-1".into(),
        events: vec![
            Event::OrderAccepted {
                order_id: "o1".into(),
            },
            Event::OrderRested {
                order_id: "o1".into(),
                qty_remaining: 2,
            },
        ],
    };

    hub.publish_command_result(&cmd, &result).await;

    let first = subscription.receiver.recv().await.unwrap();
    let second = subscription.receiver.recv().await.unwrap();

    assert_eq!(first.event_seq, 1);
    assert_eq!(second.event_seq, 2);
    assert!(matches!(
        first.payload,
        StreamPayload::UserOrderUpdate { ref state, .. } if state == "accepted"
    ));
    assert!(matches!(
        second.payload,
        StreamPayload::UserOrderUpdate { ref state, .. } if state == "rested"
    ));
}

#[tokio::test]
async fn depth_snapshots_drop_while_critical_disconnects_slow_consumers() {
    let hub = StreamHub::new();
    let claims = AuthClaims {
        user_id: Some("u1".to_string()),
        can_market_stream: true,
    };

    let _market_sub = hub
        .subscribe(
            &claims,
            SubscriptionRequest {
                channel: ChannelKind::Market,
                market_id: Some("m1".to_string()),
                user_id: None,
                include_depth_snapshots: true,
            },
            1,
        )
        .await
        .unwrap();

    let _user_sub = hub
        .subscribe(
            &claims,
            SubscriptionRequest {
                channel: ChannelKind::User,
                market_id: None,
                user_id: Some("u1".to_string()),
                include_depth_snapshots: false,
            },
            1,
        )
        .await
        .unwrap();

    hub.publish_envelope(StreamEnvelope {
        version: 1,
        event_seq: 1,
        event_id: "d1".into(),
        source: "projection".into(),
        partition_key: "m1".into(),
        emitted_at_ms: 1,
        critical: false,
        payload: StreamPayload::MarketDepthSnapshot {
            market_id: "m1".into(),
            outcome_id: "yes".into(),
            bids: vec![(5000, 1)],
            asks: vec![(5100, 1)],
        },
    })
    .await;

    hub.publish_envelope(StreamEnvelope {
        version: 1,
        event_seq: 2,
        event_id: "d2".into(),
        source: "projection".into(),
        partition_key: "m1".into(),
        emitted_at_ms: 2,
        critical: false,
        payload: StreamPayload::MarketDepthSnapshot {
            market_id: "m1".into(),
            outcome_id: "yes".into(),
            bids: vec![(5001, 2)],
            asks: vec![(5101, 3)],
        },
    })
    .await;

    hub.publish_envelope(StreamEnvelope {
        version: 1,
        event_seq: 1,
        event_id: "u1".into(),
        source: "matcher".into(),
        partition_key: "user:u1".into(),
        emitted_at_ms: 1,
        critical: true,
        payload: StreamPayload::UserOrderUpdate {
            user_id: "u1".into(),
            market_id: "m1".into(),
            outcome_id: "yes".into(),
            order_id: "o1".into(),
            state: "accepted".into(),
            reason: None,
            qty_remaining: None,
        },
    })
    .await;

    hub.publish_envelope(StreamEnvelope {
        version: 1,
        event_seq: 2,
        event_id: "u2".into(),
        source: "matcher".into(),
        partition_key: "user:u1".into(),
        emitted_at_ms: 2,
        critical: true,
        payload: StreamPayload::UserOrderUpdate {
            user_id: "u1".into(),
            market_id: "m1".into(),
            outcome_id: "yes".into(),
            order_id: "o1".into(),
            state: "rested".into(),
            reason: None,
            qty_remaining: Some(1),
        },
    })
    .await;

    let metrics = hub.metrics_snapshot().await;
    assert_eq!(metrics.depth_dropped, 1);
    assert_eq!(metrics.slow_consumer_disconnects, 1);
    assert_eq!(metrics.active_connections, 1);
}

#[tokio::test]
async fn stream_hub_handles_connection_churn_under_load() {
    let hub = StreamHub::new();
    let claims = AuthClaims {
        user_id: Some("u1".to_string()),
        can_market_stream: true,
    };

    for i in 0..200_u64 {
        let sub = hub
            .subscribe(
                &claims,
                SubscriptionRequest {
                    channel: ChannelKind::Market,
                    market_id: Some("m1".to_string()),
                    user_id: None,
                    include_depth_snapshots: true,
                },
                8,
            )
            .await
            .unwrap();

        hub.publish_envelope(StreamEnvelope {
            version: 1,
            event_seq: i + 1,
            event_id: format!("ev-{i}"),
            source: "matcher".into(),
            partition_key: "m1".into(),
            emitted_at_ms: i as i64,
            critical: true,
            payload: StreamPayload::MarketTopOfBook {
                market_id: "m1".into(),
                outcome_id: "yes".into(),
                best_bid_czk: 5000,
                best_ask_czk: 5100,
            },
        })
        .await;

        hub.disconnect(sub.connection_id).await;
    }

    let metrics = hub.metrics_snapshot().await;
    assert_eq!(metrics.active_connections, 0);
}
