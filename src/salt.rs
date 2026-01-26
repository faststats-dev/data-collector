use chrono::{Datelike, NaiveDate, Utc};
use moka::future::Cache;
use rand::Rng;
use std::sync::LazyLock;

static DAILY_SALT: LazyLock<Cache<NaiveDate, [u8; 32]>> =
    LazyLock::new(|| Cache::builder().max_capacity(1).build());

fn generate_salt() -> [u8; 32] {
    rand::rng().random()
}

pub async fn get_daily_salt() -> [u8; 32] {
    let today = Utc::now().date_naive();
    let first_of_month = today.with_day(1).unwrap();

    DAILY_SALT
        .get_with(first_of_month, async { generate_salt() })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_get_daily_salt_consistency() {
        let salt1 = get_daily_salt().await;
        let salt2 = get_daily_salt().await;
        assert_eq!(salt1, salt2);
    }

    #[tokio::test]
    async fn test_salt_is_32_bytes() {
        let salt = get_daily_salt().await;
        assert_eq!(salt.len(), 32);
    }
}
