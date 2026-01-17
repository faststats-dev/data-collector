use parking_lot::RwLock;
use sha2::{Digest, Sha256};
use sqlx::types::Uuid;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const DEBOUNCE_WINDOW: Duration = Duration::from_secs(5);
const CLEANUP_THRESHOLD: usize = 10000;

struct DebounceEntry {
    last_seen: Instant,
}

static DEBOUNCE_CACHE: RwLock<Option<HashMap<[u8; 32], DebounceEntry>>> = RwLock::new(None);

/// Generate a debounce key from visitor UUID and URL
fn debounce_key(visitor_id: Uuid, url: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(visitor_id.as_bytes());
    hasher.update(url.as_bytes());
    hasher.finalize().into()
}

pub fn should_debounce(visitor_id: Uuid, url: &str) -> bool {
    let key = debounce_key(visitor_id, url);
    let now = Instant::now();

    {
        let guard = DEBOUNCE_CACHE.read();
        if let Some(cache) = guard.as_ref()
            && let Some(entry) = cache.get(&key)
            && now.duration_since(entry.last_seen) < DEBOUNCE_WINDOW
        {
            return true;
        }
    }

    let mut guard = DEBOUNCE_CACHE.write();
    let cache = guard.get_or_insert_with(HashMap::new);

    if let Some(entry) = cache.get(&key)
        && now.duration_since(entry.last_seen) < DEBOUNCE_WINDOW
    {
        return true; // Duplicate, should skip
    }

    if cache.len() > CLEANUP_THRESHOLD {
        cache.retain(|_, entry| now.duration_since(entry.last_seen) < DEBOUNCE_WINDOW);
    }

    cache.insert(key, DebounceEntry { last_seen: now });

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::Rng;

    fn random_uuid() -> Uuid {
        let bytes: [u8; 16] = rand::rng().random();
        Uuid::from_bytes(bytes)
    }

    #[test]
    fn test_first_request_not_debounced() {
        let unique_id = random_uuid();
        assert!(!should_debounce(unique_id, "https://example.com/page1"));
    }

    #[test]
    fn test_duplicate_request_debounced() {
        let unique_id = random_uuid();
        let url = "https://example.com/page2";

        assert!(!should_debounce(unique_id, url));
        assert!(should_debounce(unique_id, url));
    }

    #[test]
    fn test_different_urls_not_debounced() {
        let unique_id = random_uuid();

        assert!(!should_debounce(unique_id, "https://example.com/page-a"));
        assert!(!should_debounce(unique_id, "https://example.com/page-b"));
    }

    #[test]
    fn test_different_visitors_not_debounced() {
        let url = "https://example.com/shared-page";
        let id1 = random_uuid();
        let id2 = random_uuid();

        assert!(!should_debounce(id1, url));
        assert!(!should_debounce(id2, url));
    }
}
