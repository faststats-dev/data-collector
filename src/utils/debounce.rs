use moka::sync::Cache;
use sha2::{Digest, Sha256};
use sqlx::types::Uuid;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const DEBOUNCE_WINDOW: Duration = Duration::from_secs(5);
const MAX_ENTRIES: u64 = 10_000;
const MAINTENANCE_INTERVAL: u64 = 500;

static DEBOUNCE_CACHE: LazyLock<Cache<[u8; 32], ()>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(MAX_ENTRIES)
        .time_to_live(DEBOUNCE_WINDOW)
        .build()
});

static INSERT_COUNTER: AtomicU64 = AtomicU64::new(0);

fn debounce_key(visitor_id: Uuid, url: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(visitor_id.as_bytes());
    hasher.update(url.as_bytes());
    hasher.finalize().into()
}

fn should_debounce_event(event: Option<&str>) -> bool {
    matches!(event, Some("pageview" | "page_leave"))
}

pub fn should_debounce(visitor_id: Uuid, url: &str, event: Option<&str>) -> bool {
    if !should_debounce_event(event) {
        return false;
    }

    let key = debounce_key(visitor_id, url);

    let entry = DEBOUNCE_CACHE.entry(key).or_insert(());
    let is_new = entry.is_fresh();

    if is_new
        && INSERT_COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(MAINTENANCE_INTERVAL)
    {
        DEBOUNCE_CACHE.run_pending_tasks();
    }

    !is_new
}

#[cfg(test)]
mod tests {
    use super::*;

    fn random_uuid() -> Uuid {
        Uuid::new_v4()
    }

    #[test]
    fn test_first_request_not_debounced() {
        let unique_id = random_uuid();
        assert!(!should_debounce(
            unique_id,
            "https://example.com/page1",
            Some("pageview")
        ));
    }

    #[test]
    fn test_duplicate_request_debounced() {
        let unique_id = random_uuid();
        let url = "https://example.com/page2";

        assert!(!should_debounce(unique_id, url, Some("pageview")));
        assert!(should_debounce(unique_id, url, Some("pageview")));
    }

    #[test]
    fn test_different_urls_not_debounced() {
        let unique_id = random_uuid();

        assert!(!should_debounce(
            unique_id,
            "https://example.com/page-a",
            Some("pageview")
        ));
        assert!(!should_debounce(
            unique_id,
            "https://example.com/page-b",
            Some("pageview")
        ));
    }

    #[test]
    fn test_different_visitors_not_debounced() {
        let url = "https://example.com/shared-page";
        let id1 = random_uuid();
        let id2 = random_uuid();

        assert!(!should_debounce(id1, url, Some("pageview")));
        assert!(!should_debounce(id2, url, Some("pageview")));
    }

    #[test]
    fn test_non_page_events_are_never_debounced() {
        let unique_id = random_uuid();
        let url = "https://example.com/page3";

        assert!(!should_debounce(unique_id, url, Some("processImage")));
        assert!(!should_debounce(unique_id, url, Some("processImage")));
    }

    #[test]
    fn test_page_leave_is_debounced() {
        let unique_id = random_uuid();
        let url = "https://example.com/page4";

        assert!(!should_debounce(unique_id, url, Some("page_leave")));
        assert!(should_debounce(unique_id, url, Some("page_leave")));
    }

    #[test]
    fn test_missing_event_is_not_debounced() {
        let unique_id = random_uuid();
        let url = "https://example.com/page5";

        assert!(!should_debounce(unique_id, url, None));
        assert!(!should_debounce(unique_id, url, None));
    }
}
