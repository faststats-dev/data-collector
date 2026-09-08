use error_grouping::{Grouper, GroupingInput, GroupingPolicy};

pub use error_grouping::Language as ErrorLanguage;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupingMode {
    Legacy,
    #[default]
    Modern,
}

#[derive(Clone, Debug)]
pub struct ProjectGrouping {
    pub mode: GroupingMode,
    grouper: Grouper,
}

impl Default for ProjectGrouping {
    fn default() -> Self {
        Self::new(GroupingMode::Modern, GroupingPolicy::default())
            .expect("default grouping policy is valid")
    }
}

impl ProjectGrouping {
    pub(crate) fn new(
        mode: GroupingMode,
        policy: GroupingPolicy,
    ) -> Result<Self, error_grouping::InvalidPolicy> {
        Ok(Self {
            mode,
            grouper: Grouper::new(policy)?,
        })
    }

    fn grouper(&self) -> &Grouper {
        &self.grouper
    }

    #[cfg(test)]
    fn with_policy(mut self, policy: GroupingPolicy) -> Self {
        self.grouper = Grouper::new(policy).expect("test grouping policy is valid");
        self
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

    let result = grouping.grouper().group(GroupingInput {
        language,
        error_kind: error_type,
        stack: stacktrace,
    });
    if let Some(error) = result.parse_error() {
        metrics::counter!(
            "error_grouping_parse_failures_total",
            "language" => language.as_str(),
            "reason" => error.as_str()
        )
        .increment(1);
    }
    if let error_grouping::GroupingOutcome::Frames { warnings, .. } = result.outcome {
        for (present, label) in [
            (warnings.malformed_frame, "malformed_frame"),
            (warnings.truncated, "truncated"),
        ] {
            if present {
                metrics::counter!(
                    "error_grouping_parse_warnings_total",
                    "language" => language.as_str(),
                    "warning" => label
                )
                .increment(1);
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modern_policy_changes_are_part_of_the_fingerprint() {
        let default = ProjectGrouping::default();
        let kind_only = default.clone().with_policy(
            default
                .grouper
                .policy()
                .clone()
                .with_segments(error_grouping::SegmentSelection::ErrorKindOnly),
        );
        let stack = "at render (/app/main.js:10:2)";
        assert_ne!(
            group_hash(ErrorLanguage::JavaScript, "TypeError", stack, &default),
            group_hash(ErrorLanguage::JavaScript, "TypeError", stack, &kind_only),
        );
    }

    #[test]
    fn terminal_cause_frames_setting_uses_the_terminal_call_site() {
        let default = ProjectGrouping::default();
        let grouping = default.clone().with_policy(
            default
                .grouper
                .policy()
                .clone()
                .with_segments(error_grouping::SegmentSelection::TerminalCauseFrames),
        );
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
