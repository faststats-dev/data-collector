use replay_message::{ReplayChunk, ReplaySessionPatch};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub type SnapshotKey = (Uuid, i32, String, String);
pub type SessionKey = (Uuid, String, String);
const PATCH_TTL: Duration = Duration::from_secs(120);

struct PendingSnapshot {
    chunk: ReplayChunk,
    first_seen: Instant,
    last_seen: Instant,
}

pub struct SnapshotBuffer {
    pending: HashMap<SnapshotKey, PendingSnapshot>,
    idle: Duration,
    max_wait: Duration,
    max_events: usize,
}

struct PendingPatch {
    patch: ReplaySessionPatch,
    first_seen: Instant,
}

#[derive(Default)]
pub struct PatchBuffer {
    pending: HashMap<SessionKey, PendingPatch>,
}

impl PatchBuffer {
    pub fn push(&mut self, patch: ReplaySessionPatch, now: Instant) {
        self.push_at(patch, now);
    }

    pub fn push_at(&mut self, patch: ReplaySessionPatch, first_seen: Instant) {
        let key = session_key(patch.project_id, &patch.session_id, &patch.window_id);
        match self.pending.entry(key) {
            Entry::Occupied(mut current) => merge_patch(&mut current.get_mut().patch, patch),
            Entry::Vacant(entry) => {
                entry.insert(PendingPatch { patch, first_seen });
            }
        }
    }

    pub fn take(&mut self, key: &SessionKey) -> Option<ReplaySessionPatch> {
        self.pending.remove(key).map(|value| value.patch)
    }

    pub fn drain(&mut self) -> Vec<(ReplaySessionPatch, Instant)> {
        self.pending
            .drain()
            .map(|(_, value)| (value.patch, value.first_seen))
            .collect()
    }

    pub fn expire(&mut self, now: Instant) -> usize {
        let before = self.pending.len();
        self.pending
            .retain(|_, value| now.duration_since(value.first_seen) < PATCH_TTL);
        before - self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

impl SnapshotBuffer {
    pub fn new(idle: Duration, max_wait: Duration, max_events: usize) -> Self {
        Self {
            pending: HashMap::new(),
            idle,
            max_wait,
            max_events,
        }
    }

    pub fn push(&mut self, chunk: ReplayChunk, now: Instant) -> Option<SnapshotKey> {
        let key = snapshot_key(&chunk);
        let entry = self
            .pending
            .entry(key.clone())
            .or_insert_with(|| PendingSnapshot {
                chunk: empty_like(&chunk),
                first_seen: now,
                last_seen: now,
            });
        merge(&mut entry.chunk, chunk);
        entry.last_seen = now;
        (entry.chunk.is_final || entry.chunk.events.len() >= self.max_events).then_some(key)
    }

    pub fn ready(&self, now: Instant) -> Vec<SnapshotKey> {
        self.pending
            .iter()
            .filter(|(_, value)| {
                now.duration_since(value.last_seen) >= self.idle
                    || now.duration_since(value.first_seen) >= self.max_wait
            })
            .map(|(key, _)| key.clone())
            .collect()
    }

    pub fn take(&mut self, key: &SnapshotKey) -> Option<ReplayChunk> {
        self.pending.remove(key).map(|value| value.chunk)
    }

    pub fn restore(&mut self, chunk: ReplayChunk, now: Instant) {
        self.pending.insert(
            snapshot_key(&chunk),
            PendingSnapshot {
                chunk,
                first_seen: now,
                last_seen: now,
            },
        );
    }

    pub fn keys(&self) -> Vec<SnapshotKey> {
        self.pending.keys().cloned().collect()
    }
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

pub fn snapshot_key(chunk: &ReplayChunk) -> SnapshotKey {
    (
        chunk.project_id,
        chunk.storage_generation,
        chunk.session_id.clone(),
        chunk.window_id.clone(),
    )
}

pub fn session_key(project_id: Uuid, session_id: &str, window_id: &str) -> SessionKey {
    (project_id, session_id.to_owned(), window_id.to_owned())
}

pub fn merge_patch(current: &mut ReplaySessionPatch, next: ReplaySessionPatch) {
    current.has_errors |= next.has_errors;
    current.has_poor_vitals |= next.has_poor_vitals;
}

fn empty_like(chunk: &ReplayChunk) -> ReplayChunk {
    let mut value = chunk.clone();
    value.events.clear();
    value.client_batch_count = 0;
    value.first_sequence = None;
    value.last_sequence = None;
    value.is_final = false;
    value
}

pub fn merge(current: &mut ReplayChunk, next: ReplayChunk) {
    let current_first = current.first_sequence.unwrap_or(current.sequence);
    let next_first = next.first_sequence.unwrap_or(next.sequence);
    let next_is_newer = next_first >= current_first;
    current.first_sequence = Some(current_first.min(next_first));
    current.last_sequence = Some(
        current
            .last_sequence
            .unwrap_or(current.sequence)
            .max(next.last_sequence.unwrap_or(next.sequence)),
    );
    current.sequence = current.first_sequence.unwrap_or(current.sequence);
    current.client_batch_count = current
        .client_batch_count
        .saturating_add(next.client_batch_count.max(1));
    current.is_final |= next.is_final;
    current.flush_reason = Some("coalesced:kafka".into());
    current.batch_id = None;
    current.session_start_ms = current.session_start_ms.or(next.session_start_ms);
    if next_is_newer {
        current.view_id = next.view_id.or(current.view_id.take());
        current.identifier = next.identifier.or(current.identifier.take());
        current.browser = next.browser.or(current.browser.take());
        current.country = next.country.or(current.country.take());
        current.os = next.os.or(current.os.take());
        current.url = next.url.or(current.url.take());
    }
    current.events.extend(next.events);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn waits_for_idle_and_caps_total_wait() {
        let now = Instant::now();
        let mut buffer = SnapshotBuffer::new(Duration::from_secs(5), Duration::from_secs(20), 100);
        buffer.push(sample(1, false), now);
        assert!(buffer.ready(now + Duration::from_secs(4)).is_empty());
        buffer.push(sample(2, false), now + Duration::from_secs(4));
        assert!(buffer.ready(now + Duration::from_secs(8)).is_empty());
        assert_eq!(buffer.ready(now + Duration::from_secs(9)).len(), 1);
    }

    #[test]
    fn repeated_snapshots_cannot_extend_the_absolute_deadline() {
        let now = Instant::now();
        let mut buffer = SnapshotBuffer::new(Duration::from_secs(5), Duration::from_secs(20), 100);
        buffer.push(sample(1, false), now);
        buffer.push(sample(2, false), now + Duration::from_secs(16));
        assert!(buffer.ready(now + Duration::from_secs(19)).is_empty());
        assert_eq!(buffer.ready(now + Duration::from_secs(20)).len(), 1);
    }

    #[test]
    fn session_patches_coalesce_flags() {
        let now = Instant::now();
        let mut buffer = PatchBuffer::default();
        buffer.push(patch(true, false), now);
        buffer.push(patch(false, true), now + Duration::from_secs(1));

        let merged = buffer.take(&session_key(Uuid::nil(), "s", "w")).unwrap();
        assert!(merged.has_errors);
        assert!(merged.has_poor_vitals);
    }

    #[test]
    fn final_snapshot_is_ready_and_merged() {
        let now = Instant::now();
        let mut buffer = SnapshotBuffer::new(Duration::from_secs(5), Duration::from_secs(20), 100);
        buffer.push(sample(1, false), now);
        let key = buffer.push(sample(2, true), now).unwrap();
        let chunk = buffer.take(&key).unwrap();
        assert_eq!(chunk.events.len(), 2);
        assert_eq!(chunk.client_batch_count, 2);
        assert!(chunk.is_final);
    }

    fn sample(sequence: i64, is_final: bool) -> ReplayChunk {
        ReplayChunk {
            project_id: Uuid::nil(),
            storage_generation: 1,
            session_id: "s".into(),
            window_id: "w".into(),
            view_id: None,
            session_start_ms: None,
            is_final,
            flush_reason: None,
            batch_id: Some(sequence.to_string()),
            sequence,
            first_sequence: None,
            last_sequence: None,
            client_batch_count: 1,
            identifier: None,
            browser: None,
            country: None,
            os: None,
            url: None,
            events: vec![json!({"timestamp": sequence})],
        }
    }

    fn patch(has_errors: bool, has_poor_vitals: bool) -> ReplaySessionPatch {
        ReplaySessionPatch {
            project_id: Uuid::nil(),
            session_id: "s".into(),
            window_id: "w".into(),
            has_errors,
            has_poor_vitals,
        }
    }
}
