use serde::Deserialize;
use serde::de::Error as _;
use serde_json::Value;

use crate::schema::{
    AdoptedStyleSheetData, AssetData, CanvasMutationData, CustomData, CustomElementData, FontData,
    FullSnapshotData, InputData, MediaInteractionData, MetaData, MouseInteractionData,
    MouseMoveData, MutationData, PluginData, ScrollData, SelectionData, StyleDeclarationData,
    StyleSheetRuleData, ViewportResizeData,
};

/// Validates an already parsed JSON value as one rrweb event.
///
/// Validation borrows the existing JSON tree and does not construct an owned
/// top-level event.
///
/// # Errors
///
/// Returns an error when the value does not match the rrweb event schema.
pub fn validate_event(value: &Value) -> serde_json::Result<()> {
    let event = value
        .as_object()
        .ok_or_else(|| serde_json::Error::custom("rrweb event must be an object"))?;
    let timestamp = event
        .get("timestamp")
        .and_then(Value::as_f64)
        .ok_or_else(|| serde_json::Error::custom("timestamp must be a number"))?;
    if !timestamp.is_finite() || timestamp < 0.0 {
        return Err(serde_json::Error::custom(
            "timestamp must be a finite, non-negative number",
        ));
    }
    if let Some(delay) = event.get("delay").filter(|delay| !delay.is_null()) {
        let delay = delay
            .as_f64()
            .ok_or_else(|| serde_json::Error::custom("delay must be a number"))?;
        if !delay.is_finite() {
            return Err(serde_json::Error::custom("delay must be a finite number"));
        }
    }

    let kind = event
        .get("type")
        .and_then(Value::as_u64)
        .and_then(|kind| u8::try_from(kind).ok())
        .ok_or_else(|| serde_json::Error::custom("rrweb event type must be an integer"))?;
    let data = event
        .get("data")
        .ok_or_else(|| serde_json::Error::custom("rrweb event data is required"))?;

    match kind {
        0 | 1 => Ok(()),
        2 => FullSnapshotData::deserialize(data).map(|_| ()),
        3 => validate_incremental_data(data),
        4 => MetaData::deserialize(data).map(|_| ()),
        5 => CustomData::deserialize(data).map(|_| ()),
        6 => PluginData::deserialize(data).map(|_| ()),
        7 => AssetData::deserialize(data).map(|_| ()),
        kind => Err(serde_json::Error::custom(format!(
            "unknown rrweb event type {kind}"
        ))),
    }
}

fn validate_incremental_data(value: &Value) -> serde_json::Result<()> {
    let source = value
        .get("source")
        .and_then(Value::as_u64)
        .ok_or_else(|| serde_json::Error::custom("incremental event source must be an integer"))?;

    match source {
        0 => MutationData::deserialize(value).map(|_| ()),
        1 | 6 | 12 => MouseMoveData::deserialize(value).map(|_| ()),
        2 => MouseInteractionData::deserialize(value).map(|_| ()),
        3 => ScrollData::deserialize(value).map(|_| ()),
        4 => ViewportResizeData::deserialize(value).map(|_| ()),
        5 => InputData::deserialize(value).map(|_| ()),
        7 => MediaInteractionData::deserialize(value).map(|_| ()),
        8 => StyleSheetRuleData::deserialize(value).map(|_| ()),
        9 => CanvasMutationData::deserialize(value).map(|_| ()),
        10 => FontData::deserialize(value).map(|_| ()),
        13 => StyleDeclarationData::deserialize(value).map(|_| ()),
        14 => SelectionData::deserialize(value).map(|_| ()),
        15 => AdoptedStyleSheetData::deserialize(value).map(|_| ()),
        16 => CustomElementData::deserialize(value).map(|_| ()),
        source => Err(serde_json::Error::custom(format!(
            "unknown rrweb incremental source {source}"
        ))),
    }
}

/// Returns whether an already parsed JSON value is a valid rrweb event.
#[must_use]
pub fn is_valid_event(value: &Value) -> bool {
    validate_event(value).is_ok()
}
