use crate::fingerprint::{self, FingerprintOptions};
use crate::{Fingerprint, Language, ParseError};

/// Complete input needed to derive an error-group identifier.
#[derive(Clone, Copy, Debug)]
pub struct GroupingInput<'a> {
    pub language: Language,
    pub error_kind: &'a str,
    pub stack: &'a str,
}

/// Strength of the evidence that contributed to a fingerprint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GroupingEvidence {
    /// A recognized stack trace contributed normalized frames.
    ParsedStack,
    /// Parsing failed, so the exact non-empty raw stack kept errors separated.
    RawStack,
    /// No stack was available; only language and error kind contributed.
    ErrorKind,
}

/// Fingerprint plus enough provenance to observe degraded grouping.
#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use]
pub struct GroupingResult {
    pub fingerprint: Fingerprint,
    pub evidence: GroupingEvidence,
    pub parse_error: Option<ParseError>,
}

/// Group an error using a fixed, bounded policy.
///
/// Unrecognized non-empty stacks are hashed exactly rather than collapsing all
/// errors of the same kind into one low-confidence group.
pub fn group(input: GroupingInput<'_>) -> GroupingResult {
    group_with_options(input, FingerprintOptions::default())
}

pub(crate) fn group_with_options(
    input: GroupingInput<'_>,
    options: FingerprintOptions<'_>,
) -> GroupingResult {
    if input.stack.trim().is_empty() {
        return GroupingResult {
            fingerprint: fingerprint::kind_only(input.language, input.error_kind, options),
            evidence: GroupingEvidence::ErrorKind,
            parse_error: None,
        };
    }

    match input.language.parse_stack(input.stack) {
        Ok(trace) => GroupingResult {
            fingerprint: fingerprint::parsed(input.language, &trace, input.error_kind, options),
            evidence: GroupingEvidence::ParsedStack,
            parse_error: None,
        },
        Err(error) => GroupingResult {
            fingerprint: fingerprint::raw_stack(
                input.language,
                input.error_kind,
                input.stack,
                options,
            ),
            evidence: GroupingEvidence::RawStack,
            parse_error: Some(error),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognized_stack_reports_parsed_evidence() {
        let result = group(GroupingInput {
            language: Language::JavaScript,
            error_kind: "TypeError",
            stack: "at render (/app/main.js:10:2)",
        });
        assert_eq!(result.evidence, GroupingEvidence::ParsedStack);
    }

    #[test]
    fn empty_stack_reports_error_kind_evidence() {
        let result = group(GroupingInput {
            language: Language::Java,
            error_kind: "java.lang.RuntimeException",
            stack: " \t",
        });
        assert_eq!(result.evidence, GroupingEvidence::ErrorKind);
    }

    #[test]
    fn unrecognized_stack_reports_raw_evidence_and_reason() {
        let result = group(GroupingInput {
            language: Language::Java,
            error_kind: "java.lang.RuntimeException",
            stack: "not a stack",
        });
        assert_eq!(
            (result.evidence, result.parse_error),
            (GroupingEvidence::RawStack, Some(ParseError::Unrecognized))
        );
    }

    #[test]
    fn different_unrecognized_stacks_do_not_merge() {
        let first = group(GroupingInput {
            language: Language::Java,
            error_kind: "java.lang.RuntimeException",
            stack: "first unsupported stack",
        });
        let second = group(GroupingInput {
            language: Language::Java,
            error_kind: "java.lang.RuntimeException",
            stack: "second unsupported stack",
        });
        assert_ne!(first.fingerprint, second.fingerprint);
    }

    #[test]
    fn options_can_ignore_error_kind() {
        let input = |error_kind| GroupingInput {
            language: Language::JavaScript,
            error_kind,
            stack: "at load (/app.js:1:1)",
        };
        let options = FingerprintOptions {
            include_error_kind: false,
            ..FingerprintOptions::default()
        };

        assert_eq!(
            group_with_options(input("TypeError"), options).fingerprint,
            group_with_options(input("RangeError"), options).fingerprint
        );
    }

    #[test]
    fn options_can_ignore_raw_stack_fallback() {
        let input = |stack| GroupingInput {
            language: Language::Java,
            error_kind: "Error",
            stack,
        };
        let options = FingerprintOptions {
            include_raw_stack: false,
            ..FingerprintOptions::default()
        };

        assert_eq!(
            group_with_options(input("first unsupported stack"), options).fingerprint,
            group_with_options(input("second unsupported stack"), options).fingerprint
        );
    }
}
