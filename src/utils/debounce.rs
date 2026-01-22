use moka::future::Cache;
use sha2::{Digest, Sha256};
use sqlx::types::Uuid;
use std::sync::LazyLock;
use std::time::Duration;

const DEBOUNCE_WINDOW: Duration = Duration::from_secs(30);
const MAX_ENTRIES: u64 = 50_000;

static DEBOUNCE_CACHE: LazyLock<Cache<[u8; 32], ()>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(MAX_ENTRIES)
        .time_to_live(DEBOUNCE_WINDOW)
        .build()
});

fn debounce_key(visitor_id: Uuid, url: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(visitor_id.as_bytes());
    hasher.update(url.as_bytes());
    hasher.finalize().into()
}

pub async fn should_debounce(visitor_id: Uuid, url: &str) -> bool {
    let key = debounce_key(visitor_id, url);

    if DEBOUNCE_CACHE.contains_key(&key) {
        return true;
    }

    DEBOUNCE_CACHE.insert(key, ()).await;
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

    #[tokio::test]
    async fn test_first_request_not_debounced() {
        let unique_id = random_uuid();
        assert!(!should_debounce(unique_id, "https://example.com/page1").await);
    }

    #[tokio::test]
    async fn test_duplicate_request_debounced() {
        let unique_id = random_uuid();
        let url = "https://example.com/page2";

        assert!(!should_debounce(unique_id, url).await);
        assert!(should_debounce(unique_id, url).await);
    }

    #[tokio::test]
    async fn test_different_urls_not_debounced() {
        let unique_id = random_uuid();

        assert!(!should_debounce(unique_id, "https://example.com/page-a").await);
        assert!(!should_debounce(unique_id, "https://example.com/page-b").await);
    }

    #[tokio::test]
    async fn test_different_visitors_not_debounced() {
        let url = "https://example.com/shared-page";
        let id1 = random_uuid();
        let id2 = random_uuid();

        assert!(!should_debounce(id1, url).await);
        assert!(!should_debounce(id2, url).await);
    }
}
