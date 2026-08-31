use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const SCHEMA_VERSION: u16 = 1;
pub const TOPIC_ENV: &str = "COLLECTOR_KAFKA_TOPIC";
pub const DEFAULT_TOPIC: &str = "collector-events-v1";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Message {
    pub schema_version: u16,
    pub message_id: Uuid,
    #[serde(flatten)]
    pub payload: Payload,
}

impl Message {
    pub fn new(payload: Payload) -> Self {
        let message_id = match &payload {
            Payload::WebEvent(row) => row.id,
            Payload::ModsEvent(row) => row.id,
            Payload::WebVital(row) => row.id,
            Payload::ErrorOccurrence(row) => Uuid::new_v5(
                &Uuid::NAMESPACE_URL,
                format!(
                    "faststats:error:{}:{}:{}:{}:{}",
                    row.project_id,
                    row.timestamp.timestamp_millis(),
                    row.exact_hash,
                    row.identifier,
                    row.session_id
                )
                .as_bytes(),
            ),
        };
        Self {
            schema_version: SCHEMA_VERSION,
            message_id,
            payload,
        }
    }

    pub fn key(&self) -> String {
        match &self.payload {
            Payload::WebEvent(row) => format!("{}:{}", row.project_id, row.id),
            Payload::ModsEvent(row) => format!("{}:{}", row.project_id, row.id),
            Payload::WebVital(row) => format!("{}:{}", row.project_id, row.id),
            Payload::ErrorOccurrence(row) => {
                format!("{}:{}:{}", row.project_id, row.group_hash, self.message_id)
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Payload {
    WebEvent(WebEvent),
    ModsEvent(ModsEvent),
    WebVital(WebVital),
    ErrorOccurrence(ErrorOccurrence),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WebEvent {
    pub id: Uuid,
    pub project_id: Uuid,
    pub user_id: Option<String>,
    pub person_id: Option<String>,
    pub external_id: Option<String>,
    pub is_identified: bool,
    pub session_id: Option<String>,
    pub event: Option<String>,
    pub browser: Option<String>,
    pub browser_version: Option<String>,
    pub device: Option<String>,
    pub os: Option<String>,
    pub os_version: Option<String>,
    pub referrer: Option<String>,
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
    pub utm_term: Option<String>,
    pub utm_content: Option<String>,
    pub title: Option<String>,
    pub page: Option<String>,
    pub url: Option<String>,
    pub country: Option<String>,
    pub cookieless: Option<bool>,
    pub time_on_page: Option<u64>,
    pub session_duration: Option<u64>,
    pub properties: String,
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModsEvent {
    pub id: Uuid,
    pub project_id: Uuid,
    pub server_id: Uuid,
    pub player_count: Option<f64>,
    pub online_mode: Option<bool>,
    pub client: Option<bool>,
    pub plugin_version: Option<String>,
    pub minecraft_version: Option<String>,
    pub server_type: Option<String>,
    pub platform_version: Option<String>,
    pub java_version: Option<String>,
    pub java_vendor: Option<String>,
    pub os_name: Option<String>,
    pub os_arch: Option<String>,
    pub os_version: Option<String>,
    pub core_count: Option<u16>,
    pub country: Option<String>,
    pub custom: String,
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ErrorOccurrence {
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub timestamp: DateTime<Utc>,
    pub project_id: Uuid,
    pub environment: String,
    pub language: String,
    pub release: String,
    pub group_hash: String,
    pub exact_hash: String,
    pub error_type: String,
    pub error_message: String,
    pub handled: bool,
    pub stacktrace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mapped_stacktrace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mapping_used: Option<String>,
    pub identifier: String,
    pub session_id: String,
    pub window_id: String,
    pub sdk_name: String,
    pub sdk_version: String,
    pub count: u32,
    pub context: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WebVital {
    pub id: Uuid,
    pub project_id: Uuid,
    pub metric: String,
    pub value: f64,
    pub device: Option<String>,
    pub country: Option<String>,
    pub os: Option<String>,
    pub os_version: Option<String>,
    pub browser: Option<String>,
    pub browser_version: Option<String>,
    pub url: String,
    pub attributes: String,
    pub session_id: Option<String>,
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web_event(id: Uuid) -> WebEvent {
        WebEvent {
            id,
            project_id: Uuid::nil(),
            user_id: None,
            person_id: None,
            external_id: None,
            is_identified: false,
            session_id: None,
            event: Some("pageview".into()),
            browser: None,
            browser_version: None,
            device: None,
            os: None,
            os_version: None,
            referrer: None,
            utm_source: None,
            utm_medium: None,
            utm_campaign: None,
            utm_term: None,
            utm_content: None,
            title: None,
            page: None,
            url: None,
            country: None,
            cookieless: None,
            time_on_page: None,
            session_duration: None,
            properties: "{}".into(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn event_ids_are_stable_across_retries() {
        let id = Uuid::new_v4();
        let first = Message::new(Payload::WebEvent(web_event(id)));
        let retry = Message::new(Payload::WebEvent(web_event(id)));
        assert_eq!(first.message_id, retry.message_id);
    }

    #[test]
    fn envelope_is_versioned_and_tagged() {
        let value =
            serde_json::to_value(Message::new(Payload::WebEvent(web_event(Uuid::new_v4()))))
                .unwrap();
        assert_eq!(value["schema_version"], SCHEMA_VERSION);
        assert_eq!(value["type"], "web_event");
        assert!(value["data"].is_object());
    }
}
