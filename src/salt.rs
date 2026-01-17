use chrono::{NaiveDate, Utc};
use rand::Rng;
use std::sync::RwLock;

struct SaltState {
    salt: String,
    date: NaiveDate,
}

static DAILY_SALT: RwLock<Option<SaltState>> = RwLock::new(None);

fn generate_salt() -> String {
    let bytes: [u8; 32] = rand::rng().random();
    hex::encode(bytes)
}

pub fn get_daily_salt() -> String {
    let today = Utc::now().date_naive();

    {
        let guard = DAILY_SALT.read().unwrap();
        if let Some(state) = guard.as_ref()
            && state.date == today {
                return state.salt.clone();
            }
    }

    let mut guard = DAILY_SALT.write().unwrap();

    if let Some(state) = guard.as_ref()
        && state.date == today {
            return state.salt.clone();
        }

    let new_salt = generate_salt();
    *guard = Some(SaltState {
        salt: new_salt.clone(),
        date: today,
    });

    new_salt
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
    fn test_salt_is_64_hex_chars() {
        let salt = get_daily_salt();
        assert_eq!(salt.len(), 64);
        assert!(salt.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
