//! Serde-backed validation for rrweb event payloads.
//!
//! Based on rrweb's canonical
//! [`packages/types/src/index.ts`](https://github.com/rrweb-io/rrweb/blob/main/packages/types/src/index.ts)
//! type definitions.
//!
//! The validator checks numeric discriminants and their associated payloads
//! while allowing additional fields for forward compatibility.

mod schema;
mod validation;

pub use schema::*;
pub use validation::{is_valid_event, validate_event};
