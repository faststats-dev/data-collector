use chrono::{NaiveDate, Utc};
use parking_lot::RwLock;
use rand::Rng;

struct SaltState {
    salt: [u8; 32],
    date: NaiveDate,
}

static DAILY_SALT: RwLock<Option<SaltState>> = RwLock::new(None);

fn generate_salt() -> [u8; 32] {
    rand::rng().random()
}

pub fn get_daily_salt() -> [u8; 32] {
    let today = Utc::now().date_naive();

    {
        let guard = DAILY_SALT.read();
        if let Some(state) = guard.as_ref()
            && state.date == today
        {
            return state.salt;
        }
    }

    let mut guard = DAILY_SALT.write();

    if let Some(state) = guard.as_ref()
        && state.date == today
    {
        return state.salt;
    }

    let new_salt = generate_salt();
    *guard = Some(SaltState {
        salt: new_salt,
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
    fn test_salt_is_32_bytes() {
        let salt = get_daily_salt();
        assert_eq!(salt.len(), 32);
    }
}
