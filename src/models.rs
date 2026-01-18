use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DataSource {
    pub reference_id: String,
    pub name: String,
    pub data_type: String,
    pub regex: Option<String>,
    pub allow_negative: Option<bool>,
    pub allow_float: Option<bool>,
    pub min_value: Option<f64>,
    pub max_value: Option<f64>,
    pub is_array: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(flatten)]
    pub id: RequestIdentifier,
    pub data: HashMap<String, Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum RequestIdentifier {
    #[serde(rename = "server_id")]
    ServerId(String),
    #[serde(rename = "identifier")]
    Identifier(String),
}

impl RequestIdentifier {
    pub fn value(&self) -> &str {
        match self {
            RequestIdentifier::ServerId(s) => s,
            RequestIdentifier::Identifier(s) => s,
        }
    }
}
