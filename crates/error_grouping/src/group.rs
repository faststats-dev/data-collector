use crate::ast::ParserLimits;
use crate::fingerprint::{self, FingerprintOptions};
use crate::{Fingerprint, Language, ParseError};

/// Internal policy boundary for grouping behavior.
///
/// Keeping parsing and fingerprint selection together lets a future compiled
/// user configuration borrow its rules for each event without cloning them.
#[derive(Debug, Default)]
struct GroupingOptions<'a> {
    parser_limits: ParserLimits,
    fingerprint: FingerprintOptions<'a>,
}

/// Complete input needed to derive an error-group identifier.
#[derive(Clone, Copy, Debug)]
pub struct GroupingInput<'a> {
    /// Runtime that produced the stack trace.
    pub language: Language,
    /// Authoritative error or exception type supplied by the SDK.
    pub error_kind: &'a str,
    /// Raw stack trace, or an empty string when unavailable.
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
    /// Stable, versioned identity for storage and comparison.
    pub fingerprint: Fingerprint,
    /// Source material used to derive the fingerprint.
    pub evidence: GroupingEvidence,
    /// Parser failure that caused a raw-stack fallback, if any.
    pub parse_error: Option<ParseError>,
}

/// Group an error using a fixed, bounded policy.
///
/// Unrecognized non-empty stacks are hashed exactly rather than collapsing all
/// errors of the same kind into one low-confidence group.
pub fn group(input: GroupingInput<'_>) -> GroupingResult {
    group_with_options(input, &GroupingOptions::default())
}

fn group_with_options(input: GroupingInput<'_>, options: &GroupingOptions<'_>) -> GroupingResult {
    if input.stack.trim().is_empty() {
        return GroupingResult {
            fingerprint: fingerprint::kind_only(
                input.language,
                input.error_kind,
                options.fingerprint,
            ),
            evidence: GroupingEvidence::ErrorKind,
            parse_error: None,
        };
    }

    match input
        .language
        .parse_stack_with_limits(input.stack, &options.parser_limits)
    {
        Ok(trace) => GroupingResult {
            fingerprint: fingerprint::parsed(
                input.language,
                &trace,
                input.error_kind,
                options.fingerprint,
            ),
            evidence: GroupingEvidence::ParsedStack,
            parse_error: None,
        },
        Err(error) => GroupingResult {
            fingerprint: fingerprint::raw_stack(
                input.language,
                input.error_kind,
                input.stack,
                options.fingerprint,
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
        let mut options = GroupingOptions::default();
        options.fingerprint.include_error_kind = false;

        assert_eq!(
            group_with_options(input("TypeError"), &options).fingerprint,
            group_with_options(input("RangeError"), &options).fingerprint
        );
    }

    #[test]
    fn options_can_ignore_raw_stack_fallback() {
        let input = |stack| GroupingInput {
            language: Language::Java,
            error_kind: "Error",
            stack,
        };
        let mut options = GroupingOptions::default();
        options.fingerprint.include_raw_stack = false;

        assert_eq!(
            group_with_options(input("first unsupported stack"), &options).fingerprint,
            group_with_options(input("second unsupported stack"), &options).fingerprint
        );
    }

    #[test]
    fn parser_limits_are_part_of_grouping_options() {
        let mut options = GroupingOptions::default();
        options.parser_limits.max_input_bytes = 4;

        let result = group_with_options(
            GroupingInput {
                language: Language::Java,
                error_kind: "Error",
                stack: "at a.B.run(B.java:1)",
            },
            &options,
        );

        assert_eq!(result.evidence, GroupingEvidence::RawStack);
        assert!(matches!(
            result.parse_error,
            Some(ParseError::InputTooLarge { limit: 4, .. })
        ));
    }
}
