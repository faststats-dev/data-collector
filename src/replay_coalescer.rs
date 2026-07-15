use crate::replay_storage::{ReplayChunkInput, ReplayStorage, ReplayStorageError};
use sqlx::types::Uuid;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, warn};

const DEFAULT_CHANNEL_CAPACITY: usize = 2_000;
const DEFAULT_MAX_AGE: Duration = Duration::from_secs(15);
const DEFAULT_MAX_BYTES: usize = 1024 * 1024;
const DEFAULT_MAX_CHUNKS_PER_WINDOW: usize = 64;
const DEFAULT_MAX_WINDOWS: usize = 10_000;

pub struct ReplayCoalescer {
    tx: mpsc::Sender<ReplayCommand>,
}

enum ReplayCommand {
    Ingest(Box<ReplayChunkInput>),
    FlushAll(oneshot::Sender<Result<(), String>>),
}

struct ReplayCoalescerActor {
    storage: Arc<ReplayStorage>,
    pool: sqlx::PgPool,
    buffers: HashMap<ReplayCoalescerKey, ReplayBuffer>,
    max_age: Duration,
    max_bytes: usize,
    max_chunks_per_window: usize,
    max_windows: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReplayCoalescerKey {
    project_id: Uuid,
    storage_generation: i32,
    session_id: String,
    window_id: String,
}

struct ReplayBuffer {
    chunks: Vec<ReplayChunkInput>,
    first_seen: Instant,
    approx_bytes: usize,
    batch_ids: HashSet<String>,
    sequences: HashSet<i64>,
}

impl ReplayCoalescer {
    pub fn new(storage: Arc<ReplayStorage>, pool: sqlx::PgPool) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(usize_env(
            "REPLAY_COALESCE_CHANNEL_CAPACITY",
            DEFAULT_CHANNEL_CAPACITY,
        ));

        ReplayCoalescerActor {
            storage,
            pool,
            buffers: HashMap::new(),
            max_age: duration_env("REPLAY_COALESCE_MAX_AGE_MS", DEFAULT_MAX_AGE),
            max_bytes: usize_env("REPLAY_COALESCE_MAX_BYTES", DEFAULT_MAX_BYTES),
            max_chunks_per_window: usize_env(
                "REPLAY_COALESCE_MAX_CHUNKS_PER_WINDOW",
                DEFAULT_MAX_CHUNKS_PER_WINDOW,
            ),
            max_windows: usize_env("REPLAY_COALESCE_MAX_WINDOWS", DEFAULT_MAX_WINDOWS),
        }
        .start(rx);

        Arc::new(Self { tx })
    }

    pub fn ingest(&self, input: ReplayChunkInput) -> Result<(), ReplayStorageError> {
        self.tx
            .try_send(ReplayCommand::Ingest(Box::new(input)))
            .map_err(|error| {
                metrics::counter!("replay_coalescer_backpressure_total").increment(1);
                ReplayStorageError::Backpressure(error.to_string())
            })
    }

    pub async fn flush_all(&self) -> Result<(), ReplayStorageError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ReplayCommand::FlushAll(tx))
            .await
            .map_err(|error| ReplayStorageError::Backpressure(error.to_string()))?;
        rx.await
            .map_err(|error| ReplayStorageError::Backpressure(error.to_string()))?
            .map_err(ReplayStorageError::Backpressure)
    }
}

impl ReplayCoalescerActor {
    fn start(mut self, mut rx: mpsc::Receiver<ReplayCommand>) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    command = rx.recv() => {
                        match command {
                            Some(ReplayCommand::Ingest(input)) => {
                                if let Err(error) = self.ingest(*input).await {
                                    error!("Failed to coalesce replay chunk: {}", error);
                                }
                            }
                            Some(ReplayCommand::FlushAll(reply)) => {
                                let result = self.flush_all().await.map_err(|error| error.to_string());
                                let _ = reply.send(result);
                            }
                            None => {
                                if let Err(error) = self.flush_all().await {
                                    error!("Failed to flush replay coalescer before shutdown: {}", error);
                                }
                                break;
                            }
                        }
                    }
                    _ = interval.tick() => {
                        if let Err(error) = self.flush_expired().await {
                            error!("Failed to flush expired replay buffers: {}", error);
                        }
                    }
                }
            }
        });
    }

    async fn ingest(&mut self, input: ReplayChunkInput) -> Result<(), ReplayStorageError> {
        let mut flush_buffers = Vec::new();
        let input_key = key(&input);

        if self.buffers.len() >= self.max_windows
            && !self.buffers.contains_key(&input_key)
            && let Some(oldest_key) = self.oldest_key()
            && let Some(buffer) = self.buffers.remove(&oldest_key)
        {
            warn!("Replay coalescer window limit reached; flushing oldest buffer");
            flush_buffers.push(buffer);
        }

        let buffer = self
            .buffers
            .entry(input_key.clone())
            .or_insert_with(ReplayBuffer::new);

        if !buffer.push(input) {
            return Ok(());
        }

        if buffer.should_flush(self.max_age, self.max_bytes, self.max_chunks_per_window)
            && let Some(buffer) = self.buffers.remove(&input_key)
        {
            flush_buffers.push(buffer);
        }

        self.flush_buffers(flush_buffers).await
    }

    async fn flush_expired(&mut self) -> Result<(), ReplayStorageError> {
        let expired_keys = self
            .buffers
            .iter()
            .filter(|(_, buffer)| {
                buffer.should_flush(self.max_age, self.max_bytes, self.max_chunks_per_window)
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();

        let buffers = expired_keys
            .into_iter()
            .filter_map(|key| self.buffers.remove(&key))
            .collect::<Vec<_>>();

        self.record_buffer_metrics();
        self.flush_buffers(buffers).await
    }

    async fn flush_all(&mut self) -> Result<(), ReplayStorageError> {
        let buffers = self.buffers.drain().map(|(_, buffer)| buffer).collect();
        self.record_buffer_metrics();
        self.flush_buffers(buffers).await
    }

    async fn flush_buffers(
        &mut self,
        buffers: Vec<ReplayBuffer>,
    ) -> Result<(), ReplayStorageError> {
        let mut first_error = None;

        for mut buffer in buffers {
            if buffer.chunks.is_empty() {
                continue;
            }

            if !chunks_are_sequence_ordered(&buffer.chunks) {
                buffer.chunks.sort_by_key(|chunk| chunk.sequence);
            }
            let buffer_key = buffer.key();
            let mut bundle = bundle_chunks(buffer.chunks);

            if let Err(error) = self
                .storage
                .store_replay_chunk(&self.pool, &mut bundle)
                .await
            {
                self.buffers
                    .entry(buffer_key)
                    .or_insert_with(|| ReplayBuffer::from_input(bundle));
                if first_error.is_none() {
                    first_error = Some(error);
                }
                continue;
            }

            metrics::counter!("replay_coalescer_bundles_flushed_total").increment(1);
            metrics::histogram!("replay_coalescer_bundle_client_batches")
                .record(bundle.client_batch_count as f64);
            metrics::histogram!("replay_coalescer_bundle_approx_bytes")
                .record(bundle.approx_events_bytes as f64);
        }

        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    }

    fn oldest_key(&self) -> Option<ReplayCoalescerKey> {
        self.buffers
            .iter()
            .min_by_key(|(_, buffer)| buffer.first_seen)
            .map(|(key, _)| key.clone())
    }

    fn record_buffer_metrics(&self) {
        metrics::gauge!("replay_coalescer_active_buffers").set(self.buffers.len() as f64);
        metrics::gauge!("replay_coalescer_buffered_approx_bytes").set(
            self.buffers
                .values()
                .map(|buffer| buffer.approx_bytes)
                .sum::<usize>() as f64,
        );
    }
}

impl ReplayBuffer {
    fn new() -> Self {
        Self {
            chunks: Vec::new(),
            first_seen: Instant::now(),
            approx_bytes: 0,
            batch_ids: HashSet::new(),
            sequences: HashSet::new(),
        }
    }

    fn push(&mut self, input: ReplayChunkInput) -> bool {
        if let Some(batch_id) = input.batch_id.as_ref()
            && !self.batch_ids.insert(batch_id.clone())
        {
            return false;
        }
        if !self.sequences.insert(input.sequence) {
            return false;
        }
        self.approx_bytes = self.approx_bytes.saturating_add(input.approx_events_bytes);
        self.chunks.push(input);
        true
    }

    fn from_input(input: ReplayChunkInput) -> Self {
        let mut buffer = Self::new();
        buffer.push(input);
        buffer
    }

    fn should_flush(&self, max_age: Duration, max_bytes: usize, max_chunks: usize) -> bool {
        self.chunks.iter().any(|chunk| chunk.is_final)
            || self.first_seen.elapsed() >= max_age
            || self.approx_bytes >= max_bytes
            || self.chunks.len() >= max_chunks
    }

    fn key(&self) -> ReplayCoalescerKey {
        key(self.chunks.first().expect("non-empty replay buffer"))
    }
}

fn key(input: &ReplayChunkInput) -> ReplayCoalescerKey {
    ReplayCoalescerKey {
        project_id: input.project_id,
        storage_generation: input.storage_generation,
        session_id: input.session_id.clone(),
        window_id: input.window_id.clone(),
    }
}

fn bundle_chunks(chunks: Vec<ReplayChunkInput>) -> ReplayChunkInput {
    let first = chunks.first().expect("non-empty replay buffer");
    let project_id = first.project_id;
    let storage_generation = first.storage_generation;
    let session_id = first.session_id.clone();
    let window_id = first.window_id.clone();
    let view_id = first.view_id.clone();
    let first_sequence = first.sequence;
    let last_sequence = chunks.last().map_or(first_sequence, |chunk| chunk.sequence);
    let client_batch_count = chunks
        .iter()
        .filter(|chunk| chunk.batch_id.is_some())
        .count()
        .max(1);

    let mut session_start_ms = first.session_start_ms;
    let mut events = Vec::new();
    let mut is_final = false;
    let mut identifier = None;
    let mut url = None;
    let mut flush_reasons = Vec::new();
    let mut events_size_bytes = 0usize;

    for mut chunk in chunks {
        is_final |= chunk.is_final;
        events_size_bytes = events_size_bytes.saturating_add(chunk.approx_events_bytes);
        identifier = chunk.identifier.or(identifier);
        url = chunk.url.or(url);
        session_start_ms = match (session_start_ms, chunk.session_start_ms) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (None, value) | (value, None) => value,
        };
        if let Some(reason) = chunk.flush_reason.take() {
            flush_reasons.push(reason);
        }
        events.append(&mut chunk.events);
    }

    ReplayChunkInput {
        project_id,
        storage_generation,
        batch_id: Some(format!(
            "bundle:{session_id}:{first_sequence}:{last_sequence}"
        )),
        session_id,
        window_id,
        view_id,
        session_start_ms,
        is_final,
        flush_reason: Some(if flush_reasons.is_empty() {
            "coalesced".to_string()
        } else {
            format!("coalesced:{}", flush_reasons.join(","))
        }),
        sequence: first_sequence,
        first_sequence: Some(first_sequence),
        last_sequence: Some(last_sequence),
        client_batch_count: i32::try_from(client_batch_count).unwrap_or(i32::MAX),
        approx_events_bytes: events_size_bytes,
        identifier,
        url,
        events,
    }
}

fn chunks_are_sequence_ordered(chunks: &[ReplayChunkInput]) -> bool {
    chunks
        .windows(2)
        .all(|pair| pair[0].sequence <= pair[1].sequence)
}

fn usize_env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn duration_env(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(default)
}
