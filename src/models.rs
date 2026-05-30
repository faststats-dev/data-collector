use crate::batch_queue::BatchQueue;
use crate::replay_storage::ReplayStorage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub batch_queue: Arc<BatchQueue>,
    pub replay_storage: Option<Arc<ReplayStorage>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataSource {
    pub reference_id: String,
    pub name: String,
    pub data_type: String,
    pub regex: Option<String>,
    pub allow_negative: Option<bool>,
    pub allow_float: Option<bool>,
    pub min_value: Option<f64>,
    pub max_value: Option<f64>,
    pub metric_shape: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Error {
    pub error: String,
    pub message: Option<String>,
    pub stack: Option<Vec<String>>,
    #[serde(default)]
    pub cause: Option<Box<Error>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorTracking {
    #[serde(flatten)]
    pub error: Error,
    #[serde(default)]
    pub count: Option<i32>,
    #[serde(default, rename = "buildId")]
    pub build_id: Option<String>,
    #[serde(default)]
    pub context: Option<Value>,
    #[serde(default, rename = "sdkVersion")]
    pub sdk_version: Option<String>,
    #[serde(default, skip_deserializing)]
    pub session_id: Option<String>,
    pub handled: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(flatten)]
    pub id: RequestIdentifier,
    pub data: HashMap<String, Value>,
    pub errors: Option<Vec<ErrorTracking>>,
    #[serde(default)]
    pub context: Option<Value>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_web_request_parsing() {
        #[derive(serde::Deserialize, Debug)]
        struct WebRequest {
            errors: Option<Vec<ErrorTracking>>,
            #[serde(rename = "sessionId")]
            session_id: Option<String>,
        }

        let json = r#"{
            "token": "f2a2b1b24d739f57daa73ba95e4076da",
            "identifier": "f2a2b1b24d739f57daa73ba95e4076da",
            "data": {
                "url": "http://localhost:5174/errors",
                "page": "/errors"
            },
            "errors": [
                {
                    "error": "Error",
                    "message": "Uncaught Error: Render error",
                    "stack": ["line1", "line2"],
                    "count": 1
                }
            ],
            "sessionId": "mkqsr2zu-rhhe8v3j"
        }"#;

        let result = serde_json::from_str::<WebRequest>(json);
        println!("Parse result: {:?}", result);
        assert!(result.is_ok(), "Failed to parse: {:?}", result.err());

        let req = result.unwrap();
        assert!(req.errors.is_some());
        let errors = req.errors.unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].error.error, "Error");
        assert_eq!(errors[0].handled, None);
        assert_eq!(req.session_id, Some("mkqsr2zu-rhhe8v3j".to_string()));
    }

    #[test]
    fn test_error_tracking_handled_parsing() {
        let json = r#"{
            "hash": "err_3d39cc9f28fb81e8b7064481c7deb8c0bb349cb0877558cc73b677c1fb9a704d",
            "error": "Error",
            "message": "Uncaught Error: Render error",
            "context": { "component": "checkout" },
            "handled": true
        }"#;

        let result = serde_json::from_str::<ErrorTracking>(json);
        assert!(result.is_ok(), "Failed to parse: {:?}", result.err());
        let error = result.unwrap();
        assert_eq!(error.handled, Some(true));
        assert_eq!(
            error.context,
            Some(serde_json::json!({ "component": "checkout" }))
        );
    }

    #[test]
    fn test_collect_request_parses_root_context() {
        let json = r#"{
            "server_id": "f2a2b1b2-4d73-49f5-9daa-73ba95e4076d",
            "data": {},
            "context": { "region": "eu" },
            "errors": [
                {
                    "error": "Error",
                    "message": "Render error",
                    "context": { "component": "checkout" }
                }
            ]
        }"#;

        let result = serde_json::from_str::<Request>(json);
        assert!(result.is_ok(), "Failed to parse: {:?}", result.err());

        let request = result.unwrap();
        assert_eq!(request.context, Some(serde_json::json!({ "region": "eu" })));
        assert_eq!(
            request
                .errors
                .as_ref()
                .and_then(|errors| errors.first())
                .and_then(|error| error.context.as_ref()),
            Some(&serde_json::json!({ "component": "checkout" }))
        );
    }
}
