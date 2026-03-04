use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct KafkaRecord {
    pub topic: String,
    pub partition_key: String,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct DeadLetterRecord {
    pub record: KafkaRecord,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub enum KafkaPublishError {
    Transient(String),
    Permanent(String),
}

#[tonic::async_trait]
pub trait KafkaTransport: Send + Sync {
    async fn publish(&self, record: KafkaRecord) -> Result<(), KafkaPublishError>;
}

#[derive(Default)]
pub struct KafkaMetrics {
    published: std::sync::atomic::AtomicU64,
    retries: std::sync::atomic::AtomicU64,
    dead_lettered: std::sync::atomic::AtomicU64,
}

#[derive(Debug, Clone)]
pub struct KafkaMetricsSnapshot {
    pub published: u64,
    pub retries: u64,
    pub dead_lettered: u64,
}

impl KafkaMetrics {
    pub fn snapshot(&self) -> KafkaMetricsSnapshot {
        KafkaMetricsSnapshot {
            published: self.published.load(std::sync::atomic::Ordering::Relaxed),
            retries: self.retries.load(std::sync::atomic::Ordering::Relaxed),
            dead_lettered: self
                .dead_lettered
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }
}

pub struct KafkaProducer<T: KafkaTransport> {
    transport: T,
    max_retries: usize,
    retry_backoff: Duration,
    metrics: Arc<KafkaMetrics>,
    dead_letters: Arc<Mutex<Vec<DeadLetterRecord>>>,
}

impl<T: KafkaTransport> KafkaProducer<T> {
    pub fn new(transport: T, max_retries: usize, retry_backoff: Duration) -> Self {
        Self {
            transport,
            max_retries,
            retry_backoff,
            metrics: Arc::new(KafkaMetrics::default()),
            dead_letters: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn metrics(&self) -> Arc<KafkaMetrics> {
        self.metrics.clone()
    }

    pub async fn dead_letters(&self) -> Vec<DeadLetterRecord> {
        self.dead_letters.lock().await.clone()
    }

    pub async fn publish(&self, record: KafkaRecord) -> Result<(), KafkaPublishError> {
        let mut attempts = 0usize;
        loop {
            match self.transport.publish(record.clone()).await {
                Ok(()) => {
                    self.metrics
                        .published
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return Ok(());
                }
                Err(KafkaPublishError::Transient(_reason)) if attempts < self.max_retries => {
                    attempts += 1;
                    self.metrics
                        .retries
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tokio::time::sleep(self.retry_backoff).await;
                    continue;
                }
                Err(KafkaPublishError::Transient(reason)) => {
                    self.push_dead_letter(record, format!("transient exhausted: {reason}"))
                        .await;
                    return Err(KafkaPublishError::Transient(reason));
                }
                Err(KafkaPublishError::Permanent(reason)) => {
                    self.push_dead_letter(record, format!("permanent: {reason}"))
                        .await;
                    return Err(KafkaPublishError::Permanent(reason));
                }
            }
        }
    }

    async fn push_dead_letter(&self, record: KafkaRecord, reason: String) {
        self.dead_letters
            .lock()
            .await
            .push(DeadLetterRecord { record, reason });
        self.metrics
            .dead_lettered
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}
