pub use error_grouping::Language as ErrorLanguage;

pub(crate) fn parse_optional(
    value: Option<&str>,
) -> Result<ErrorLanguage, error_grouping::UnsupportedLanguage> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(error_grouping::UnsupportedLanguage)?
        .parse()
}

pub(crate) fn group_hash(language: ErrorLanguage, error_type: &str, stacktrace: &str) -> String {
    let result = error_grouping::group(error_grouping::GroupingInput {
        language,
        error_kind: error_type,
        stack: stacktrace,
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_language_is_rejected() {
        assert!(parse_optional(None).is_err());
        assert!(parse_optional(Some("")).is_err());
    }

    #[test]
    fn unsupported_language_is_rejected() {
        assert!(parse_optional(Some("ruby")).is_err());
    }

    #[test]
    fn authoritative_type_changes_the_group() {
        let stack = "at render (/app/main.js:10:2)";
        let type_error = group_hash(ErrorLanguage::JavaScript, "TypeError", stack);
        let range_error = group_hash(ErrorLanguage::JavaScript, "RangeError", stack);

        assert!(type_error.starts_with("eg1_"));
        assert_ne!(type_error, range_error);
    }

    #[test]
    fn header_and_frame_only_payloads_group_together() {
        let frames = group_hash(
            ErrorLanguage::JavaScript,
            "TypeError",
            "at render (/app/main.js:10:2)",
        );
        let full = group_hash(
            ErrorLanguage::JavaScript,
            "TypeError",
            "TypeError: dynamic message\nat render (/app/main.js:99:8)",
        );

        assert_eq!(frames, full);
    }

    #[test]
    fn empty_and_header_only_payloads_group_together() {
        let empty = group_hash(ErrorLanguage::Java, "java.lang.RuntimeException", "");
        let header = group_hash(
            ErrorLanguage::Java,
            "java.lang.RuntimeException",
            "java.lang.RuntimeException: dynamic message",
        );

        assert_eq!(empty, header);
    }

    #[test]
    fn unparsed_stacks_use_raw_stack_evidence() {
        let first = group_hash(ErrorLanguage::Java, "FirstError", "not a stack");
        let second = group_hash(ErrorLanguage::Java, "SecondError", "not a stack");
        let noisy = group_hash(
            ErrorLanguage::Java,
            "FirstError",
            "different unsupported text",
        );

        assert_ne!(first, second);
        assert_ne!(first, noisy);
    }
}
