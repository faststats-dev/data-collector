use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

const DEBOUNCE_WINDOW: Duration = Duration::from_secs(5);
const CLEANUP_THRESHOLD: usize = 10000;

struct DebounceEntry {
    last_seen: Instant,
}

static DEBOUNCE_CACHE: RwLock<Option<HashMap<[u8; 32], DebounceEntry>>> = RwLock::new(None);

/// Generate a debounce key from visitor ID and URL
fn debounce_key(visitor_id: &str, url: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(visitor_id.as_bytes());
    hasher.update(b"|");
    hasher.update(url.as_bytes());
    hasher.finalize().into()
}

pub fn should_debounce(visitor_id: &str, url: &str) -> bool {
    let key = debounce_key(visitor_id, url);
    let now = Instant::now();

    {
        let guard = DEBOUNCE_CACHE.read().unwrap();
        if let Some(cache) = guard.as_ref()
            && let Some(entry) = cache.get(&key)
            && now.duration_since(entry.last_seen) < DEBOUNCE_WINDOW
        {
            return true;
        }
    }

    let mut guard = DEBOUNCE_CACHE.write().unwrap();
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

    #[test]
    fn test_first_request_not_debounced() {
        let unique_id = format!("test-{}", Instant::now().elapsed().as_nanos());
        assert!(!should_debounce(&unique_id, "https://example.com/page1"));
    }

    #[test]
    fn test_duplicate_request_debounced() {
        let unique_id = format!("test-dup-{}", Instant::now().elapsed().as_nanos());
        let url = "https://example.com/page2";

        assert!(!should_debounce(&unique_id, url));
        assert!(should_debounce(&unique_id, url));
    }

    #[test]
    fn test_different_urls_not_debounced() {
        let unique_id = format!("test-urls-{}", Instant::now().elapsed().as_nanos());

        assert!(!should_debounce(&unique_id, "https://example.com/page-a"));
        assert!(!should_debounce(&unique_id, "https://example.com/page-b"));
    }

    #[test]
    fn test_different_visitors_not_debounced() {
        let url = "https://example.com/shared-page";
        let id1 = format!("visitor1-{}", Instant::now().elapsed().as_nanos());
        let id2 = format!("visitor2-{}", Instant::now().elapsed().as_nanos());

        assert!(!should_debounce(&id1, url));
        assert!(!should_debounce(&id2, url));
    }
}
