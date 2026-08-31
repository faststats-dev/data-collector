use error_grouping::{
    AdjacentFramePolicy, ErrorKindPolicy, FrameExclusion, FrameField, FrameFields, FrameMatcher,
    FramePolicy, GroupingInput, GroupingPolicy, ParserLimits, RawStackPolicy, RuntimeFramePolicy,
    SegmentSelection,
};

pub use error_grouping::Language as ErrorLanguage;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupingMode {
    Legacy,
    #[default]
    Modern,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ProjectGrouping {
    pub mode: GroupingMode,
    pub parser_max_input_bytes: usize,
    pub parser_max_lines: usize,
    pub parser_max_line_bytes: usize,
    pub segment_selection: String,
    pub include_error_kind: bool,
    pub raw_stack_policy: String,
    pub raw_stack_max_bytes: usize,
    pub max_frames: usize,
    pub include_function: bool,
    pub include_module: bool,
    pub include_file: bool,
    pub runtime_frame_policy: String,
    pub adjacent_frame_policy: String,
    pub exclusions: Vec<OwnedFrameExclusion>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct OwnedFrameExclusion {
    pub field: String,
    pub matcher: String,
    pub pattern: String,
}

impl Default for ProjectGrouping {
    fn default() -> Self {
        Self {
            mode: GroupingMode::Modern,
            parser_max_input_bytes: 1_048_576,
            parser_max_lines: 16_384,
            parser_max_line_bytes: 65_536,
            segment_selection: "root_and_terminal_cause".to_owned(),
            include_error_kind: true,
            raw_stack_policy: "bounded".to_owned(),
            raw_stack_max_bytes: 1_048_576,
            max_frames: 8,
            include_function: true,
            include_module: true,
            include_file: true,
            runtime_frame_policy: "exclude_when_application_frame_exists".to_owned(),
            adjacent_frame_policy: "deduplicate".to_owned(),
            exclusions: Vec::new(),
        }
    }
}

pub(crate) fn parse_optional_language(
    value: Option<&str>,
) -> Result<ErrorLanguage, error_grouping::UnsupportedLanguage> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map_or(Ok(ErrorLanguage::Java), str::parse)
}

pub(crate) fn group_hash(
    language: ErrorLanguage,
    error_type: &str,
    stacktrace: &str,
    grouping: &ProjectGrouping,
) -> String {
    if grouping.mode == GroupingMode::Legacy
        && let Some(language) = legacy_language(language)
    {
        return legacy_grouping::group(language, error_type, stacktrace);
    }

    let exclusions = grouping
        .exclusions
        .iter()
        .filter_map(OwnedFrameExclusion::as_rule)
        .collect::<Vec<_>>();
    let mut fields = FrameFields::NONE;
    if grouping.include_function {
        fields = fields.union(FrameFields::FUNCTION);
    }
    if grouping.include_module {
        fields = fields.union(FrameFields::MODULE);
    }
    if grouping.include_file {
        fields = fields.union(FrameFields::FILE);
    }
    let frame_policy = FramePolicy::default()
        .with_max_frames(grouping.max_frames)
        .with_fields(fields)
        .with_runtime_frames(match grouping.runtime_frame_policy.as_str() {
            "include" => RuntimeFramePolicy::Include,
            _ => RuntimeFramePolicy::ExcludeWhenApplicationFrameExists,
        })
        .with_adjacent_frames(match grouping.adjacent_frame_policy.as_str() {
            "preserve" => AdjacentFramePolicy::Preserve,
            _ => AdjacentFramePolicy::Deduplicate,
        })
        .with_exclusions(&exclusions);
    let policy = GroupingPolicy::default()
        .with_parser_limits(ParserLimits {
            max_input_bytes: grouping.parser_max_input_bytes,
            max_lines: grouping.parser_max_lines,
            max_line_bytes: grouping.parser_max_line_bytes,
        })
        .with_segments(match grouping.segment_selection.as_str() {
            "error_kind_only" => SegmentSelection::ErrorKindOnly,
            "root" => SegmentSelection::Root,
            "terminal_cause_frames" => SegmentSelection::TerminalCauseFrames,
            _ => SegmentSelection::RootAndTerminalCause,
        })
        .with_error_kind(if grouping.include_error_kind {
            ErrorKindPolicy::Include
        } else {
            ErrorKindPolicy::Ignore
        })
        .with_raw_stack(match grouping.raw_stack_policy.as_str() {
            "error_kind_only" => RawStackPolicy::ErrorKindOnly,
            _ => RawStackPolicy::Bounded {
                max_bytes: grouping.raw_stack_max_bytes,
            },
        })
        .with_frames(frame_policy);
    let result = error_grouping::group_with_policy(
        GroupingInput {
            language,
            error_kind: error_type,
            stack: stacktrace,
        },
        &policy,
    );
    if let Some(error) = &result.parse_error {
        metrics::counter!(
            "error_grouping_parse_failures_total",
            "language" => language.as_str(),
            "reason" => error.as_str()
        )
        .increment(1);
    }
    result.fingerprint.to_string()
}

fn legacy_language(language: ErrorLanguage) -> Option<legacy_grouping::Language> {
    match language {
        ErrorLanguage::Java => Some(legacy_grouping::Language::Java),
        ErrorLanguage::JavaScript => Some(legacy_grouping::Language::JavaScript),
        ErrorLanguage::Php => Some(legacy_grouping::Language::Php),
        ErrorLanguage::Rust => Some(legacy_grouping::Language::Rust),
        ErrorLanguage::Python | ErrorLanguage::Go | ErrorLanguage::Swift => None,
        _ => None,
    }
}

impl OwnedFrameExclusion {
    fn as_rule(&self) -> Option<FrameExclusion<'_>> {
        let field = match self.field.as_str() {
            "function" => FrameField::Function,
            "module" => FrameField::Module,
            "file" => FrameField::File,
            _ => return None,
        };
        let matcher = match self.matcher.as_str() {
            "exact" => FrameMatcher::Exact(&self.pattern),
            "prefix" => FrameMatcher::Prefix(&self.pattern),
            "suffix" => FrameMatcher::Suffix(&self.pattern),
            "contains" => FrameMatcher::Contains(&self.pattern),
            _ => return None,
        };
        Some(FrameExclusion::new(field, matcher))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modern_policy_changes_are_part_of_the_fingerprint() {
        let default = ProjectGrouping::default();
        let mut kind_only = default.clone();
        kind_only.segment_selection = "error_kind_only".to_owned();
        let stack = "at render (/app/main.js:10:2)";
        assert_ne!(
            group_hash(ErrorLanguage::JavaScript, "TypeError", stack, &default),
            group_hash(ErrorLanguage::JavaScript, "TypeError", stack, &kind_only),
        );
    }

    #[test]
    fn terminal_cause_frames_setting_uses_the_terminal_call_site() {
        let mut grouping = ProjectGrouping::default();
        grouping.segment_selection = "terminal_cause_frames".to_owned();
        let missing_method = "Wrapper: failed\n at server.First.run(First.java:1)\nCaused by: java.lang.NoSuchMethodError\n at app.Plugin.load(Plugin.java:10)";
        let missing_field = "Wrapper: failed\n at server.Second.run(Second.java:1)\nCaused by: java.lang.NoSuchFieldError\n at app.Plugin.load(Plugin.java:20)";

        assert_eq!(
            group_hash(
                ErrorLanguage::Java,
                "com.example.Wrapper",
                missing_method,
                &grouping,
            ),
            group_hash(
                ErrorLanguage::Java,
                "com.example.Wrapper",
                missing_field,
                &grouping,
            ),
        );
    }

    #[test]
    fn legacy_mode_preserves_the_removed_hash() {
        let grouping = ProjectGrouping {
            mode: GroupingMode::Legacy,
            ..ProjectGrouping::default()
        };
        assert_eq!(
            group_hash(
                ErrorLanguage::Java,
                "java.lang.RuntimeException",
                "\tat plugin-1.2.3.jar//com.example.Plugin.handle(Plugin.java:42)",
                &grouping,
            ),
            "f06e38f4eff0dc1f77c5408fa596935cd875fe0baea8672153c82d3362337219",
        );
    }
}
