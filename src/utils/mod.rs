pub mod debounce;

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// SHA256 hash the server_id with the project_id to produce a deterministic UUID.
pub fn hash_server_id(server_id: Uuid, project_id: Uuid) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(server_id.as_bytes());
    hasher.update(project_id.as_bytes());
    let hash = hasher.finalize();
    Uuid::from_slice(&hash[..16]).unwrap()
}

/// GDPR-compliant daily-rotating hash for cookieless tracking.
/// Produces a deterministic UUID from IP + User-Agent + project_id + today's date.
/// The hash rotates daily so visitors cannot be tracked long-term,
/// and the original IP/UA cannot be recovered.
pub fn cookieless_server_id(ip: &str, user_agent: &str, project_id: Uuid) -> Uuid {
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let mut hasher = Sha256::new();
    hasher.update(ip.as_bytes());
    hasher.update(user_agent.as_bytes());
    hasher.update(project_id.as_bytes());
    hasher.update(today.as_bytes());
    let hash = hasher.finalize();
    Uuid::from_slice(&hash[..16]).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn hash_server_id_is_deterministic() {
        let server_id = Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap();
        let project_id = Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap();

        let result = Uuid::parse_str("43de3f42-d8de-0da8-1a6c-6c58fcd8b6e0").unwrap();

        let first = hash_server_id(server_id, project_id);
        let second = hash_server_id(server_id, project_id);

        assert_eq!(first, second);
        assert_eq!(first, result);
        assert_eq!(second, result);
    }

    #[test]
    fn hash_server_id_changes_with_input() {
        let server_id = Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap();
        let project_id = Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap();
        let other_project_id = Uuid::parse_str("cccccccc-cccc-cccc-cccc-cccccccccccc").unwrap();

        let original = hash_server_id(server_id, project_id);
        let different = hash_server_id(server_id, other_project_id);

        assert_ne!(original, different);
    }

    #[test]
    fn cookieless_server_id_is_deterministic() {
        let project_id = Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap();
        let first = cookieless_server_id("1.2.3.4", "Mozilla/5.0", project_id);
        let second = cookieless_server_id("1.2.3.4", "Mozilla/5.0", project_id);
        assert_eq!(first, second);
    }

    #[test]
    fn cookieless_server_id_differs_by_ip() {
        let project_id = Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap();
        let a = cookieless_server_id("1.2.3.4", "Mozilla/5.0", project_id);
        let b = cookieless_server_id("5.6.7.8", "Mozilla/5.0", project_id);
        assert_ne!(a, b);
    }

    #[test]
    fn cookieless_server_id_differs_by_user_agent() {
        let project_id = Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap();
        let a = cookieless_server_id("1.2.3.4", "Mozilla/5.0", project_id);
        let b = cookieless_server_id("1.2.3.4", "Chrome/120", project_id);
        assert_ne!(a, b);
    }

    #[test]
    fn cookieless_server_id_differs_by_project() {
        let project_a = Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap();
        let project_b = Uuid::parse_str("cccccccc-cccc-cccc-cccc-cccccccccccc").unwrap();
        let a = cookieless_server_id("1.2.3.4", "Mozilla/5.0", project_a);
        let b = cookieless_server_id("1.2.3.4", "Mozilla/5.0", project_b);
        assert_ne!(a, b);
    }

    #[test]
    fn cookieless_server_id_differs_from_hash_server_id() {
        let id = Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap();
        let project_id = Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap();
        let hashed = hash_server_id(id, project_id);
        let cookieless = cookieless_server_id("1.2.3.4", "Mozilla/5.0", project_id);
        assert_ne!(hashed, cookieless);
    }

    #[test]
    fn cookieless_server_id_handles_empty_inputs() {
        let project_id = Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap();
        let result = cookieless_server_id("", "", project_id);
        // Should still produce a valid UUID, not panic
        assert_ne!(result, Uuid::nil());
    }
}
