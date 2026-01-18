use chrono::{NaiveDate, Utc};
use moka::sync::Cache;
use rand::Rng;
use std::sync::LazyLock;

static DAILY_SALT: LazyLock<Cache<NaiveDate, [u8; 32]>> =
    LazyLock::new(|| Cache::builder().max_capacity(1).build());

fn generate_salt() -> [u8; 32] {
    rand::rng().random()
}

pub fn get_daily_salt() -> [u8; 32] {
    let today = Utc::now().date_naive();

    DAILY_SALT.get_with(today, generate_salt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_daily_salt_consistency() {
        let salt1 = get_daily_salt();
        let salt2 = get_daily_salt();
        assert_eq!(salt1, salt2);
    }

    #[test]
    fn test_salt_is_32_bytes() {
        let salt = get_daily_salt();
        assert_eq!(salt.len(), 32);
    }
}
