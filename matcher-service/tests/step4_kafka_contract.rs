use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use matcher_service::kafka::{KafkaProducer, KafkaPublishError, KafkaRecord, KafkaTransport};
use matcher_service::stream_pb::event_envelope::Payload;
use matcher_service::stream_pb::{EventEnvelope, MarketTrade};
use prost::Message;
use tokio::sync::Mutex;

#[derive(Clone)]
struct ScriptedTransport {
    responses: Arc<Mutex<VecDeque<Result<(), KafkaPublishError>>>>,
}

#[tonic::async_trait]
impl KafkaTransport for ScriptedTransport {
    async fn publish(&self, _record: KafkaRecord) -> Result<(), KafkaPublishError> {
        self.responses.lock().await.pop_front().unwrap_or(Ok(()))
    }
}

#[tokio::test]
async fn kafka_producer_retries_transient_and_dead_letters_on_exhaustion() {
    let transport = ScriptedTransport {
        responses: Arc::new(Mutex::new(VecDeque::from(vec![
            Err(KafkaPublishError::Transient("timeout".into())),
            Err(KafkaPublishError::Transient("timeout".into())),
            Ok(()),
        ]))),
    };

    let producer = KafkaProducer::new(transport, 3, Duration::from_millis(1));
    producer
        .publish(KafkaRecord {
            topic: "md.trades".into(),
            partition_key: "m1".into(),
            payload: vec![1, 2, 3],
        })
        .await
        .unwrap();

    let snapshot = producer.metrics().snapshot();
    assert_eq!(snapshot.published, 1);
    assert_eq!(snapshot.retries, 2);
    assert_eq!(snapshot.dead_lettered, 0);

    let failing_transport = ScriptedTransport {
        responses: Arc::new(Mutex::new(VecDeque::from(vec![Err(
            KafkaPublishError::Permanent("bad schema".into()),
        )]))),
    };
    let failing = KafkaProducer::new(failing_transport, 2, Duration::from_millis(1));
    let err = failing
        .publish(KafkaRecord {
            topic: "md.trades".into(),
            partition_key: "m1".into(),
            payload: vec![9],
        })
        .await
        .unwrap_err();
    assert!(matches!(err, KafkaPublishError::Permanent(_)));
    assert_eq!(failing.metrics().snapshot().dead_lettered, 1);
    assert_eq!(failing.dead_letters().await.len(), 1);
}

#[test]
fn protobuf_contract_accepts_additive_unknown_fields() {
    let envelope = EventEnvelope {
        version: 1,
        event_seq: 42,
        event_id: "ev-1".into(),
        source: "matcher".into(),
        partition_key: "m1".into(),
        emitted_at_ms: 123,
        payload: Some(Payload::MarketTrade(MarketTrade {
            market_id: "m1".into(),
            outcome_id: "yes".into(),
            fill_id: "fill-1".into(),
            maker_order_id: "o1".into(),
            taker_order_id: "o2".into(),
            price_czk: 5000,
            qty: 3,
        })),
    };

    let mut encoded = envelope.encode_to_vec();

    // Unknown additive field: tag=100, wire-type=2 (length-delimited), value="future".
    encoded.extend_from_slice(&[0xA2, 0x06, 0x06, b'f', b'u', b't', b'u', b'r', b'e']);

    let decoded = EventEnvelope::decode(encoded.as_slice()).unwrap();
    assert_eq!(decoded.version, 1);
    assert_eq!(decoded.event_seq, 42);
    assert!(matches!(decoded.payload, Some(Payload::MarketTrade(_))));
}
