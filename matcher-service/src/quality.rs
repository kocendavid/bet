use std::collections::{BTreeMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::Mutex;

use crate::streaming::StreamMetricsSnapshot;
use crate::types::{Command, CommandResult, Event};

const WINDOW_1M_MS: i64 = 60_000;
const WINDOW_5M_MS: i64 = 300_000;
const WINDOW_15M_MS: i64 = 900_000;
const RETENTION_MS: i64 = WINDOW_15M_MS + 30_000;

#[derive(Debug, Clone, Serialize)]
pub struct QualityFilter {
    pub market_id: Option<String>,
    pub outcome_id: Option<String>,
    pub user_id: Option<String>,
}

#[derive(Debug, Clone)]
struct CommandQualityEvent {
    ts_ms: i64,
    market_id: String,
    outcome_id: String,
    user_id: String,
    shard: Option<usize>,
    accepted: bool,
    reject_reasons: Vec<String>,
    processing_ms: u64,
}

#[derive(Debug, Clone)]
struct StreamSample {
    ts_ms: i64,
    snapshot: StreamMetricsSnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub struct LatencySummary {
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct StreamingDelta {
    pub delivered: u64,
    pub depth_dropped: u64,
    pub slow_consumer_disconnects: u64,
    pub unauthorized: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShardBalance {
    pub processed_by_shard: Vec<u64>,
    pub imbalance_ratio: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct QualityWindowSnapshot {
    pub ingress_total: u64,
    pub accepted_total: u64,
    pub rejected_total: u64,
    pub reject_rate: f64,
    pub risk_reject_total: u64,
    pub reject_reasons: BTreeMap<String, u64>,
    pub throughput_per_sec: f64,
    pub processing_latency_ms: LatencySummary,
    pub streaming: StreamingDelta,
    pub shard_balance: ShardBalance,
    pub last_event_age_ms: Option<u64>,
    pub severity: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct QualitySnapshot {
    pub generated_at_ms: i64,
    pub filter: QualityFilter,
    #[serde(rename = "1m")]
    pub one_minute: QualityWindowSnapshot,
    #[serde(rename = "5m")]
    pub five_minutes: QualityWindowSnapshot,
    #[serde(rename = "15m")]
    pub fifteen_minutes: QualityWindowSnapshot,
}

pub struct QualityCollector {
    events: Mutex<VecDeque<CommandQualityEvent>>,
    stream_samples: Mutex<VecDeque<StreamSample>>,
}

impl Default for QualityCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl QualityCollector {
    pub fn new() -> Self {
        Self {
            events: Mutex::new(VecDeque::new()),
            stream_samples: Mutex::new(VecDeque::new()),
        }
    }

    pub async fn record_command(
        &self,
        command: &Command,
        shard: Option<usize>,
        processing_ms: u64,
        result: &CommandResult,
    ) {
        let (market_id, outcome_id, user_id) = command_identity(command);
        let mut reject_reasons = Vec::new();
        for event in &result.events {
            if let Event::OrderRejected { reason, .. } = event {
                reject_reasons.push(reason.clone());
            }
        }

        let event = CommandQualityEvent {
            ts_ms: now_ms(),
            market_id,
            outcome_id,
            user_id,
            shard,
            accepted: reject_reasons.is_empty(),
            reject_reasons,
            processing_ms,
        };

        let mut events = self.events.lock().await;
        events.push_back(event);
        prune_events(&mut events, now_ms() - RETENTION_MS);
    }

    pub async fn snapshot(
        &self,
        filter: QualityFilter,
        stream_snapshot: StreamMetricsSnapshot,
    ) -> QualitySnapshot {
        let now = now_ms();
        {
            let mut samples = self.stream_samples.lock().await;
            samples.push_back(StreamSample {
                ts_ms: now,
                snapshot: stream_snapshot,
            });
            prune_stream_samples(&mut samples, now - RETENTION_MS);
        }

        let events = self.events.lock().await.clone();
        let samples = self.stream_samples.lock().await.clone();

        QualitySnapshot {
            generated_at_ms: now,
            filter: filter.clone(),
            one_minute: window_snapshot(now, WINDOW_1M_MS, &filter, &events, &samples),
            five_minutes: window_snapshot(now, WINDOW_5M_MS, &filter, &events, &samples),
            fifteen_minutes: window_snapshot(now, WINDOW_15M_MS, &filter, &events, &samples),
        }
    }
}

fn window_snapshot(
    now: i64,
    window_ms: i64,
    filter: &QualityFilter,
    events: &VecDeque<CommandQualityEvent>,
    samples: &VecDeque<StreamSample>,
) -> QualityWindowSnapshot {
    let cutoff = now - window_ms;
    let mut ingress_total = 0_u64;
    let mut accepted_total = 0_u64;
    let mut rejected_total = 0_u64;
    let mut risk_reject_total = 0_u64;
    let mut reject_reasons = BTreeMap::new();
    let mut latencies = Vec::new();
    let mut latest_event_ts: Option<i64> = None;
    let mut by_shard = BTreeMap::<usize, u64>::new();

    for event in events {
        if event.ts_ms < cutoff || !matches_filter(event, filter) {
            continue;
        }
        ingress_total += 1;
        if event.accepted {
            accepted_total += 1;
            if let Some(shard) = event.shard {
                *by_shard.entry(shard).or_default() += 1;
            }
        } else {
            rejected_total += 1;
        }
        latencies.push(event.processing_ms);
        latest_event_ts = Some(latest_event_ts.map_or(event.ts_ms, |ts| ts.max(event.ts_ms)));

        for reason in &event.reject_reasons {
            *reject_reasons.entry(reason.clone()).or_default() += 1;
            if reason.starts_with("risk_") {
                risk_reject_total += 1;
            }
        }
    }

    let reject_rate = if ingress_total == 0 {
        0.0
    } else {
        rejected_total as f64 / ingress_total as f64
    };
    let throughput_per_sec = ingress_total as f64 / (window_ms as f64 / 1000.0);
    let processing_latency_ms = latency_summary(latencies);
    let streaming = stream_delta(cutoff, samples);
    let shard_balance = shard_balance(by_shard);
    let last_event_age_ms = latest_event_ts.map(|ts| now.saturating_sub(ts) as u64);
    let severity = severity(
        reject_rate,
        processing_latency_ms.p95,
        streaming.slow_consumer_disconnects,
        shard_balance.imbalance_ratio,
    );

    QualityWindowSnapshot {
        ingress_total,
        accepted_total,
        rejected_total,
        reject_rate,
        risk_reject_total,
        reject_reasons,
        throughput_per_sec,
        processing_latency_ms,
        streaming,
        shard_balance,
        last_event_age_ms,
        severity,
    }
}

fn matches_filter(event: &CommandQualityEvent, filter: &QualityFilter) -> bool {
    if let Some(market_id) = &filter.market_id {
        if event.market_id != *market_id {
            return false;
        }
    }
    if let Some(outcome_id) = &filter.outcome_id {
        if event.outcome_id != *outcome_id {
            return false;
        }
    }
    if let Some(user_id) = &filter.user_id {
        if event.user_id != *user_id {
            return false;
        }
    }
    true
}

fn command_identity(command: &Command) -> (String, String, String) {
    match command {
        Command::Place(place) => (
            place.market_id.clone(),
            place.outcome_id.clone(),
            place.user_id.clone(),
        ),
        Command::Cancel(cancel) => (
            cancel.market_id.clone(),
            cancel.outcome_id.clone(),
            cancel.user_id.clone(),
        ),
    }
}

fn prune_events(events: &mut VecDeque<CommandQualityEvent>, min_ts_ms: i64) {
    while events.front().is_some_and(|event| event.ts_ms < min_ts_ms) {
        events.pop_front();
    }
}

fn prune_stream_samples(samples: &mut VecDeque<StreamSample>, min_ts_ms: i64) {
    while samples
        .front()
        .is_some_and(|sample| sample.ts_ms < min_ts_ms)
    {
        samples.pop_front();
    }
}

fn latency_summary(mut values: Vec<u64>) -> LatencySummary {
    if values.is_empty() {
        return LatencySummary {
            p50: 0,
            p95: 0,
            p99: 0,
        };
    }
    values.sort_unstable();
    LatencySummary {
        p50: percentile(&values, 50.0),
        p95: percentile(&values, 95.0),
        p99: percentile(&values, 99.0),
    }
}

fn percentile(values: &[u64], p: f64) -> u64 {
    let n = values.len();
    if n == 0 {
        return 0;
    }
    let rank = ((p / 100.0) * (n as f64 - 1.0)).round() as usize;
    values[rank.min(n - 1)]
}

fn stream_delta(cutoff: i64, samples: &VecDeque<StreamSample>) -> StreamingDelta {
    let Some(last) = samples.back() else {
        return StreamingDelta::default();
    };
    let base = samples
        .iter()
        .find(|sample| sample.ts_ms >= cutoff)
        .unwrap_or(last);

    StreamingDelta {
        delivered: last
            .snapshot
            .delivered
            .saturating_sub(base.snapshot.delivered),
        depth_dropped: last
            .snapshot
            .depth_dropped
            .saturating_sub(base.snapshot.depth_dropped),
        slow_consumer_disconnects: last
            .snapshot
            .slow_consumer_disconnects
            .saturating_sub(base.snapshot.slow_consumer_disconnects),
        unauthorized: last
            .snapshot
            .unauthorized
            .saturating_sub(base.snapshot.unauthorized),
    }
}

fn shard_balance(by_shard: BTreeMap<usize, u64>) -> ShardBalance {
    if by_shard.is_empty() {
        return ShardBalance {
            processed_by_shard: Vec::new(),
            imbalance_ratio: 1.0,
        };
    }

    let max_shard = *by_shard.keys().max().unwrap_or(&0);
    let mut processed_by_shard = vec![0_u64; max_shard + 1];
    for (shard, count) in by_shard {
        processed_by_shard[shard] = count;
    }

    let non_zero: Vec<u64> = processed_by_shard
        .iter()
        .copied()
        .filter(|v| *v > 0)
        .collect();
    let imbalance_ratio = if non_zero.len() <= 1 {
        1.0
    } else {
        let min = *non_zero.iter().min().unwrap_or(&1) as f64;
        let max = *non_zero.iter().max().unwrap_or(&1) as f64;
        max / min
    };

    ShardBalance {
        processed_by_shard,
        imbalance_ratio,
    }
}

fn severity(
    reject_rate: f64,
    p95_latency_ms: u64,
    slow_consumer_disconnects: u64,
    imbalance_ratio: f64,
) -> String {
    let mut level = 0_u8; // 0=ok,1=warn,2=critical

    if reject_rate > 0.10
        || p95_latency_ms > 150
        || slow_consumer_disconnects > 0
        || imbalance_ratio > 2.0
    {
        level = 1;
    }
    if reject_rate > 0.20 || p95_latency_ms > 300 {
        level = 2;
    }

    match level {
        2 => "critical".to_string(),
        1 => "warn".to_string(),
        _ => "ok".to_string(),
    }
}

fn now_ms() -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    now.as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::{latency_summary, percentile, severity};

    #[test]
    fn percentile_basic() {
        let values = vec![10, 20, 30, 40, 50];
        assert_eq!(percentile(&values, 50.0), 30);
        assert_eq!(percentile(&values, 95.0), 50);
    }

    #[test]
    fn latency_summary_empty_is_zero() {
        let summary = latency_summary(Vec::new());
        assert_eq!(summary.p50, 0);
        assert_eq!(summary.p95, 0);
        assert_eq!(summary.p99, 0);
    }

    #[test]
    fn severity_thresholds() {
        assert_eq!(severity(0.05, 50, 0, 1.0), "ok");
        assert_eq!(severity(0.12, 50, 0, 1.0), "warn");
        assert_eq!(severity(0.05, 350, 0, 1.0), "critical");
    }
}
