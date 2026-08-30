//! Serde-backed validation for rrweb event payloads.
//!
//! Based on rrweb's canonical
//! [`packages/types/src/index.ts`](https://github.com/rrweb-io/rrweb/blob/main/packages/types/src/index.ts)
//! type definitions.
//!
//! rrweb's wire format uses numeric discriminants for events, incremental
//! sources, nodes, and interaction kinds. This crate validates those
//! discriminants and the required shape of their associated data while
//! allowing additional fields for forwards-compatible structural typing.

use serde::de::{self, DeserializeOwned};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::HashMap;

/// A structurally validated rrweb event.
#[derive(Debug)]
pub struct Event {
    /// Milliseconds since the Unix epoch.
    pub timestamp: f64,
    /// Optional replay delay added by rrweb's player.
    pub delay: Option<f64>,
    /// The validated event payload.
    pub data: EventData,
}

/// The payload associated with an rrweb event type.
#[derive(Debug)]
pub enum EventData {
    DomContentLoaded(Value),
    Load(Value),
    FullSnapshot(FullSnapshotData),
    IncrementalSnapshot(IncrementalData),
    Meta(MetaData),
    Custom(CustomData),
    Plugin(PluginData),
    Asset(AssetData),
}

/// Validate and deserialize one rrweb event from JSON bytes.
pub fn from_slice(input: &[u8]) -> serde_json::Result<Event> {
    serde_json::from_slice(input)
}

/// Validate an already parsed JSON value as one rrweb event.
pub fn validate_event(value: &Value) -> serde_json::Result<()> {
    Event::deserialize(value).map(|_| ())
}

/// Return whether an already parsed JSON value is a valid rrweb event.
pub fn is_valid_event(value: &Value) -> bool {
    validate_event(value).is_ok()
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireEvent {
            #[serde(rename = "type")]
            kind: u8,
            data: Value,
            timestamp: f64,
            #[serde(default)]
            delay: Option<f64>,
        }

        let wire = WireEvent::deserialize(deserializer)?;
        if !wire.timestamp.is_finite() || wire.timestamp < 0.0 {
            return Err(de::Error::custom(
                "timestamp must be a finite, non-negative number",
            ));
        }
        if wire.delay.is_some_and(|delay| !delay.is_finite()) {
            return Err(de::Error::custom("delay must be a finite number"));
        }

        let data = match wire.kind {
            0 => EventData::DomContentLoaded(wire.data),
            1 => EventData::Load(wire.data),
            2 => EventData::FullSnapshot(parse(wire.data)?),
            3 => EventData::IncrementalSnapshot(parse(wire.data)?),
            4 => EventData::Meta(parse(wire.data)?),
            5 => EventData::Custom(parse(wire.data)?),
            6 => EventData::Plugin(parse(wire.data)?),
            7 => EventData::Asset(parse(wire.data)?),
            kind => {
                return Err(de::Error::custom(format!(
                    "unknown rrweb event type {kind}"
                )));
            }
        };

        Ok(Self {
            timestamp: wire.timestamp,
            delay: wire.delay,
            data,
        })
    }
}

fn parse<E, T>(value: Value) -> Result<T, E>
where
    E: de::Error,
    T: DeserializeOwned,
{
    serde_json::from_value(value).map_err(E::custom)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FullSnapshotData {
    pub node: SerializedNode,
    pub initial_offset: Offset,
}

#[derive(Debug, Deserialize)]
pub struct Offset {
    pub top: f64,
    pub left: f64,
}

#[derive(Debug, Deserialize)]
pub struct MetaData {
    pub href: String,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Deserialize)]
pub struct CustomData {
    pub tag: String,
    pub payload: Value,
}

#[derive(Debug, Deserialize)]
pub struct PluginData {
    pub plugin: String,
    pub payload: Value,
}

#[derive(Debug)]
pub enum IncrementalData {
    Mutation(MutationData),
    MouseMove(MouseMoveData),
    MouseInteraction(MouseInteractionData),
    Scroll(ScrollData),
    ViewportResize(ViewportResizeData),
    Input(InputData),
    TouchMove(MouseMoveData),
    MediaInteraction(MediaInteractionData),
    StyleSheetRule(StyleSheetRuleData),
    CanvasMutation(CanvasMutationData),
    Font(FontData),
    Drag(MouseMoveData),
    StyleDeclaration(StyleDeclarationData),
    Selection(SelectionData),
    AdoptedStyleSheet(AdoptedStyleSheetData),
    CustomElement(CustomElementData),
}

impl<'de> Deserialize<'de> for IncrementalData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let source = value
            .get("source")
            .and_then(Value::as_u64)
            .ok_or_else(|| de::Error::custom("incremental event source must be an integer"))?;

        match source {
            0 => parse(value).map(Self::Mutation),
            1 => parse(value).map(Self::MouseMove),
            2 => parse(value).map(Self::MouseInteraction),
            3 => parse(value).map(Self::Scroll),
            4 => parse(value).map(Self::ViewportResize),
            5 => parse(value).map(Self::Input),
            6 => parse(value).map(Self::TouchMove),
            7 => parse(value).map(Self::MediaInteraction),
            8 => parse(value).map(Self::StyleSheetRule),
            9 => parse(value).map(Self::CanvasMutation),
            10 => parse(value).map(Self::Font),
            12 => parse(value).map(Self::Drag),
            13 => parse(value).map(Self::StyleDeclaration),
            14 => parse(value).map(Self::Selection),
            15 => parse(value).map(Self::AdoptedStyleSheet),
            16 => parse(value).map(Self::CustomElement),
            source => Err(de::Error::custom(format!(
                "unknown rrweb incremental source {source}"
            ))),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct MutationData {
    pub texts: Vec<TextMutation>,
    pub attributes: Vec<AttributeMutation>,
    pub removes: Vec<RemovedNodeMutation>,
    pub adds: Vec<AddedNodeMutation>,
    #[serde(default, rename = "isAttachIframe")]
    pub is_attach_iframe: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct TextMutation {
    pub id: i64,
    #[serde(deserialize_with = "required_option")]
    pub value: Option<String>,
}

fn required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

#[derive(Debug, Deserialize)]
pub struct AttributeMutation {
    pub id: i64,
    pub attributes: HashMap<String, MutationAttributeValue>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum MutationAttributeValue {
    String(String),
    Style(HashMap<String, StyleValue>),
    Null,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum StyleValue {
    String(String),
    ValueWithPriority((String, String)),
    Bool(#[serde(deserialize_with = "false_literal")] ()),
}

fn false_literal<'de, D>(deserializer: D) -> Result<(), D::Error>
where
    D: Deserializer<'de>,
{
    let value = bool::deserialize(deserializer)?;
    if value {
        Err(de::Error::custom("expected false"))
    } else {
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemovedNodeMutation {
    pub parent_id: i64,
    pub id: i64,
    #[serde(default)]
    pub is_shadow: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddedNodeMutation {
    pub parent_id: i64,
    #[serde(default)]
    pub previous_id: Option<i64>,
    #[serde(deserialize_with = "required_option")]
    pub next_id: Option<i64>,
    pub node: SerializedNode,
}

#[derive(Debug, Deserialize)]
pub struct MouseMoveData {
    pub positions: Vec<MousePosition>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MousePosition {
    pub x: f64,
    pub y: f64,
    pub id: i64,
    pub time_offset: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MouseInteractionData {
    #[serde(rename = "type", deserialize_with = "interaction_type")]
    pub kind: u8,
    pub id: i64,
    #[serde(default)]
    pub x: Option<f64>,
    #[serde(default)]
    pub y: Option<f64>,
    #[serde(default, deserialize_with = "optional_pointer_type")]
    pub pointer_type: Option<u8>,
}

fn interaction_type<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: Deserializer<'de>,
{
    bounded_u8(deserializer, 0, 10, "mouse interaction type")
}

fn optional_pointer_type<'de, D>(deserializer: D) -> Result<Option<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<u8>::deserialize(deserializer)?;
    match value {
        Some(value @ 0..=2) => Ok(Some(value)),
        Some(value) => Err(de::Error::custom(format!("unknown pointer type {value}"))),
        None => Ok(None),
    }
}

#[derive(Debug, Deserialize)]
pub struct ScrollData {
    pub id: i64,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Deserialize)]
pub struct ViewportResizeData {
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputData {
    pub id: i64,
    pub text: String,
    pub is_checked: bool,
    #[serde(default)]
    pub user_triggered: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaInteractionData {
    #[serde(rename = "type", deserialize_with = "media_interaction_type")]
    pub kind: u8,
    pub id: i64,
    #[serde(default)]
    pub current_time: Option<f64>,
    #[serde(default)]
    pub volume: Option<f64>,
    #[serde(default)]
    pub muted: Option<bool>,
    #[serde(default)]
    #[serde(rename = "loop")]
    pub loop_: Option<bool>,
    #[serde(default)]
    pub playback_rate: Option<f64>,
}

fn media_interaction_type<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: Deserializer<'de>,
{
    bounded_u8(deserializer, 0, 4, "media interaction type")
}

fn bounded_u8<'de, D>(deserializer: D, min: u8, max: u8, name: &str) -> Result<u8, D::Error>
where
    D: Deserializer<'de>,
{
    let value = u8::deserialize(deserializer)?;
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(de::Error::custom(format!("unknown {name} {value}")))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StyleSheetRuleData {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub style_id: Option<i64>,
    #[serde(default)]
    pub removes: Option<Vec<StyleSheetDeleteRule>>,
    #[serde(default)]
    pub adds: Option<Vec<StyleSheetAddRule>>,
    #[serde(default)]
    pub replace: Option<String>,
    #[serde(default)]
    pub replace_sync: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StyleSheetAddRule {
    pub rule: String,
    #[serde(default)]
    pub index: Option<Index>,
}

#[derive(Debug, Deserialize)]
pub struct StyleSheetDeleteRule {
    pub index: Index,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Index {
    One(u64),
    Path(Vec<u64>),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StyleDeclarationData {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub style_id: Option<i64>,
    pub index: Vec<u64>,
    #[serde(default)]
    pub set: Option<StyleDeclarationSet>,
    #[serde(default)]
    pub remove: Option<StyleDeclarationRemove>,
}

#[derive(Debug, Deserialize)]
pub struct StyleDeclarationSet {
    pub property: String,
    #[serde(deserialize_with = "required_option")]
    pub value: Option<String>,
    pub priority: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StyleDeclarationRemove {
    pub property: String,
}

#[derive(Debug)]
pub struct CanvasMutationData {
    pub id: i64,
    pub kind: u8,
    pub commands: Option<Vec<CanvasMutationCommand>>,
    pub command: Option<CanvasMutationCommand>,
}

impl<'de> Deserialize<'de> for CanvasMutationData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireCanvasMutation {
            id: i64,
            #[serde(rename = "type", deserialize_with = "canvas_context")]
            kind: u8,
            #[serde(default)]
            commands: Option<Vec<CanvasMutationCommand>>,
            #[serde(default)]
            property: Option<String>,
            #[serde(default)]
            args: Option<Vec<Value>>,
            #[serde(default)]
            setter: Option<bool>,
        }

        let wire = WireCanvasMutation::deserialize(deserializer)?;
        let command = match (wire.property, wire.args) {
            (Some(property), Some(args)) => Some(CanvasMutationCommand {
                property,
                args,
                setter: wire.setter,
            }),
            (None, None) => None,
            _ => {
                return Err(de::Error::custom(
                    "canvas mutation command requires property and args",
                ));
            }
        };
        if wire.commands.is_none() && command.is_none() {
            return Err(de::Error::custom(
                "canvas mutation requires commands or one command",
            ));
        }

        Ok(Self {
            id: wire.id,
            kind: wire.kind,
            commands: wire.commands,
            command,
        })
    }
}

fn canvas_context<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: Deserializer<'de>,
{
    bounded_u8(deserializer, 0, 2, "canvas context")
}

#[derive(Debug, Deserialize)]
pub struct CanvasMutationCommand {
    pub property: String,
    pub args: Vec<Value>,
    #[serde(default)]
    pub setter: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FontData {
    pub family: String,
    pub font_source: String,
    pub buffer: bool,
    #[serde(default)]
    pub descriptors: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct SelectionData {
    pub ranges: Vec<SelectionRange>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionRange {
    pub start: i64,
    pub start_offset: u64,
    pub end: i64,
    pub end_offset: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdoptedStyleSheetData {
    pub id: i64,
    #[serde(default)]
    pub styles: Option<Vec<AdoptedStyleSheet>>,
    pub style_ids: Vec<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdoptedStyleSheet {
    pub style_id: i64,
    pub rules: Vec<StyleSheetAddRule>,
}

#[derive(Debug, Deserialize)]
pub struct CustomElementData {
    #[serde(default)]
    pub define: Option<CustomElementDefinition>,
}

#[derive(Debug, Deserialize)]
pub struct CustomElementDefinition {
    pub name: String,
}

#[derive(Debug)]
pub enum SerializedNode {
    Document(DocumentNode),
    DocumentType(DocumentTypeNode),
    Element(ElementNode),
    Text(TextNode),
    Cdata(CdataNode),
    Comment(CommentNode),
}

impl<'de> Deserialize<'de> for SerializedNode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let kind = value
            .get("type")
            .and_then(Value::as_u64)
            .ok_or_else(|| de::Error::custom("serialized node type must be an integer"))?;
        match kind {
            0 => parse(value).map(Self::Document),
            1 => parse(value).map(Self::DocumentType),
            2 => parse(value).map(Self::Element),
            3 => parse(value).map(Self::Text),
            4 => parse(value).map(Self::Cdata),
            5 => parse(value).map(Self::Comment),
            kind => Err(de::Error::custom(format!(
                "unknown serialized node type {kind}"
            ))),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeMetadata {
    pub id: i64,
    #[serde(default)]
    pub root_id: Option<i64>,
    #[serde(default)]
    pub is_shadow_host: Option<bool>,
    #[serde(default)]
    pub is_shadow: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentNode {
    #[serde(flatten)]
    pub metadata: NodeMetadata,
    pub child_nodes: Vec<SerializedNode>,
    #[serde(default)]
    pub compat_mode: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentTypeNode {
    #[serde(flatten)]
    pub metadata: NodeMetadata,
    pub name: String,
    pub public_id: String,
    pub system_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElementNode {
    #[serde(flatten)]
    pub metadata: NodeMetadata,
    pub tag_name: String,
    pub attributes: HashMap<String, NodeAttributeValue>,
    pub child_nodes: Vec<SerializedNode>,
    #[serde(default)]
    pub is_svg: Option<bool>,
    #[serde(default)]
    pub need_block: Option<bool>,
    #[serde(default)]
    pub is_custom: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum NodeAttributeValue {
    String(String),
    Number(f64),
    True(#[serde(deserialize_with = "true_literal")] ()),
    Null,
}

fn true_literal<'de, D>(deserializer: D) -> Result<(), D::Error>
where
    D: Deserializer<'de>,
{
    let value = bool::deserialize(deserializer)?;
    if value {
        Ok(())
    } else {
        Err(de::Error::custom("expected true"))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextNode {
    #[serde(flatten)]
    pub metadata: NodeMetadata,
    pub text_content: String,
    #[serde(default)]
    pub is_style: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CdataNode {
    #[serde(flatten)]
    pub metadata: NodeMetadata,
    #[serde(deserialize_with = "empty_string")]
    pub text_content: String,
}

fn empty_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.is_empty() {
        Ok(value)
    } else {
        Err(de::Error::custom("CDATA textContent must be empty"))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommentNode {
    #[serde(flatten)]
    pub metadata: NodeMetadata,
    pub text_content: String,
}

#[derive(Debug)]
pub enum AssetData {
    Loaded(LoadedAssetData),
    Failed(FailedAssetData),
}

impl<'de> Deserialize<'de> for AssetData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        if value.get("failed").is_some() {
            parse(value).map(Self::Failed)
        } else {
            parse(value).map(Self::Loaded)
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct LoadedAssetData {
    pub url: String,
    pub payload: SerializedAsset,
    #[serde(default)]
    pub timestamp: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct FailedAssetData {
    pub url: String,
    pub failed: AssetFailure,
}

#[derive(Debug, Deserialize)]
pub struct AssetFailure {
    #[serde(default)]
    pub status: Option<u16>,
    pub message: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SerializedAsset {
    CssText {
        rr_type: CssTextMarker,
        #[serde(rename = "cssTexts")]
        css_texts: Vec<String>,
    },
    Canvas(#[serde(deserialize_with = "serialized_canvas_asset")] SerializedCanvasAsset),
}

#[derive(Debug, Deserialize)]
pub struct CssTextMarker(#[serde(deserialize_with = "css_text_marker")] ());

fn css_text_marker<'de, D>(deserializer: D) -> Result<(), D::Error>
where
    D: Deserializer<'de>,
{
    let marker = String::deserialize(deserializer)?;
    if marker == "CssText" {
        Ok(())
    } else {
        Err(de::Error::custom("expected rr_type CssText"))
    }
}

#[derive(Debug, Deserialize)]
pub struct SerializedCanvasAsset {
    pub rr_type: String,
    #[serde(default)]
    pub base64: Option<String>,
    #[serde(default)]
    pub data: Option<Vec<Value>>,
    #[serde(default, rename = "type")]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub src: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<Value>>,
    #[serde(default)]
    pub index: Option<u64>,
}

impl SerializedCanvasAsset {
    fn has_payload(&self) -> bool {
        self.base64.is_some()
            || self.data.is_some()
            || self.src.is_some()
            || self.args.is_some()
            || self.index.is_some()
    }
}

fn serialized_canvas_asset<'de, D>(deserializer: D) -> Result<SerializedCanvasAsset, D::Error>
where
    D: Deserializer<'de>,
{
    let asset = SerializedCanvasAsset::deserialize(deserializer)?;
    if asset.rr_type.is_empty() {
        return Err(de::Error::custom("canvas asset rr_type must not be empty"));
    }
    if !asset.has_payload() {
        return Err(de::Error::custom(
            "serialized canvas asset must contain data",
        ));
    }
    Ok(asset)
}

#[cfg(test)]
mod tests {
    use super::{EventData, from_slice, is_valid_event};
    use serde_json::json;

    #[test]
    fn full_snapshot_should_validate_when_node_tree_is_well_formed() {
        let event = br#"{
            "type": 2,
            "timestamp": 1710000000000,
            "data": {
                "node": {
                    "type": 0,
                    "id": 1,
                    "childNodes": [{
                        "type": 2,
                        "id": 2,
                        "tagName": "html",
                        "attributes": {},
                        "childNodes": []
                    }]
                },
                "initialOffset": { "top": 0, "left": 0 }
            }
        }"#;

        let parsed = from_slice(event).expect("valid full snapshot");

        assert!(matches!(parsed.data, EventData::FullSnapshot(_)));
    }

    #[test]
    fn mutation_should_validate_when_required_collections_exist() {
        let event = json!({
            "type": 3,
            "timestamp": 1710000000001_u64,
            "data": {
                "source": 0,
                "texts": [],
                "attributes": [],
                "removes": [],
                "adds": []
            }
        });

        assert!(is_valid_event(&event));
    }

    #[test]
    fn event_should_be_rejected_when_type_is_unknown() {
        let event = json!({ "type": 32, "timestamp": 1, "data": {} });

        assert!(!is_valid_event(&event));
    }

    #[test]
    fn full_snapshot_should_be_rejected_when_node_shape_is_invalid() {
        let event = json!({
            "type": 2,
            "timestamp": 1,
            "data": {
                "node": { "type": 2, "id": 1, "tagName": "div" },
                "initialOffset": { "top": 0, "left": 0 }
            }
        });

        assert!(!is_valid_event(&event));
    }

    #[test]
    fn incremental_event_should_be_rejected_when_source_is_unknown() {
        let event = json!({
            "type": 3,
            "timestamp": 1,
            "data": { "source": 11 }
        });

        assert!(!is_valid_event(&event));
    }

    #[test]
    fn mutation_should_reject_a_missing_required_nullable_field() {
        let event = json!({
            "type": 3,
            "timestamp": 1,
            "data": {
                "source": 0,
                "texts": [{ "id": 1 }],
                "attributes": [],
                "removes": [],
                "adds": []
            }
        });

        assert!(!is_valid_event(&event));
    }
}
