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
}
