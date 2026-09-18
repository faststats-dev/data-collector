pub use error_grouping::Language as ErrorLanguage;

pub(crate) fn parse_optional_language(
    value: Option<&str>,
) -> Result<ErrorLanguage, error_grouping::UnsupportedLanguage> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map_or(Ok(ErrorLanguage::Java), str::parse)
}

pub(crate) fn group_hash(language: ErrorLanguage, error_type: &str, stacktrace: &str) -> String {
    let legacy = match language {
        ErrorLanguage::Java => legacy_grouping::Language::Java,
        ErrorLanguage::JavaScript => legacy_grouping::Language::JavaScript,
        ErrorLanguage::Php => legacy_grouping::Language::Php,
        ErrorLanguage::Rust => legacy_grouping::Language::Rust,
        _ => return legacy_grouping::group_unparsed(language.as_str(), error_type, stacktrace),
    };
    legacy_grouping::group(legacy, error_type, stacktrace)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_historical_legacy_hash() {
        assert_eq!(
            group_hash(
                ErrorLanguage::Java,
                "java.lang.RuntimeException",
                "\tat plugin-1.2.3.jar//com.example.Plugin.handle(Plugin.java:42)"
            ),
            "f06e38f4eff0dc1f77c5408fa596935cd875fe0baea8672153c82d3362337219"
        );
    }
    #[test]
    fn every_original_language_uses_legacy_grouping() {
        for (language, legacy) in [
            (ErrorLanguage::Java, legacy_grouping::Language::Java),
            (
                ErrorLanguage::JavaScript,
                legacy_grouping::Language::JavaScript,
            ),
            (ErrorLanguage::Php, legacy_grouping::Language::Php),
            (ErrorLanguage::Rust, legacy_grouping::Language::Rust),
        ] {
            assert_eq!(
                group_hash(language, "Error", "some stack"),
                legacy_grouping::group(legacy, "Error", "some stack")
            );
        }
        for language in [
            ErrorLanguage::Python,
            ErrorLanguage::Go,
            ErrorLanguage::Swift,
        ] {
            assert_eq!(
                group_hash(language, "Error", "some stack"),
                legacy_grouping::group_unparsed(language.as_str(), "Error", "some stack")
            );
        }
    }
}
