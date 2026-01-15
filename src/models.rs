use crate::batch::BatchProcessor;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub batch_processor: Arc<BatchProcessor>,
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
pub struct Request {
    pub server_id: String,
    pub data: HashMap<String, Value>,
}
