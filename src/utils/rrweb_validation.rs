//! RRWeb event validation types - used only for deserialize validation
#![allow(dead_code)]

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct RrwebEvent {
    pub timestamp: u64,
    pub delay: Option<u64>,
    #[serde(flatten)]
    pub data: EventContent,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum EventContent {
    #[serde(rename = "0")]
    DomContentLoaded { data: serde_json::Value },
    #[serde(rename = "1")]
    Load { data: serde_json::Value },
    #[serde(rename = "2")]
    FullSnapshot { data: FullSnapshotData },
    #[serde(rename = "3")]
    IncrementalSnapshot { data: IncrementalData },
    #[serde(rename = "4")]
    Meta { data: MetaData },
    #[serde(rename = "5")]
    Custom { data: serde_json::Value },
    #[serde(rename = "6")]
    Plugin { data: serde_json::Value },
}

#[derive(Debug, Deserialize)]
pub struct MetaData {
    pub href: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Deserialize)]
pub struct FullSnapshotData {
    pub node: serde_json::Value,
    #[serde(rename = "initialOffset")]
    pub initial_offset: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct IncrementalData {
    pub source: u32,
    #[serde(flatten)]
    pub payload: std::collections::HashMap<String, serde_json::Value>,
}
