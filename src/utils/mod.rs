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
