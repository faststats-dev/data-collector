use crate::fingerprint;
use crate::{Fingerprint, GroupingPolicy, Language, ParseError};

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

/// Group an error using the default bounded policy.
///
/// Unrecognized non-empty stacks contribute bounded raw evidence rather than
/// collapsing all errors of the same kind into one low-confidence group.
pub fn group(input: GroupingInput<'_>) -> GroupingResult {
    group_with_policy(input, &GroupingPolicy::default())
}

/// Group an error with an explicit, borrow-friendly policy.
///
/// The policy identity is encoded in the returned fingerprint, so changing any
/// option cannot silently reuse identifiers produced by another policy.
pub fn group_with_policy(input: GroupingInput<'_>, policy: &GroupingPolicy<'_>) -> GroupingResult {
    let policy_id = policy.id();
    if input.stack.trim().is_empty() {
        return GroupingResult {
            fingerprint: fingerprint::kind_only(
                input.language,
                input.error_kind,
                *policy,
                policy_id,
            ),
            evidence: GroupingEvidence::ErrorKind,
            parse_error: None,
        };
    }

    match input
        .language
        .parse_stack_with_limits(input.stack, &policy.parser_limits)
    {
        Ok(trace) => {
            let evidence = if trace.has_frames() {
                GroupingEvidence::ParsedStack
            } else {
                GroupingEvidence::ErrorKind
            };
            GroupingResult {
                fingerprint: fingerprint::parsed(
                    input.language,
                    &trace,
                    input.error_kind,
                    *policy,
                    policy_id,
                ),
                evidence,
                parse_error: None,
            }
        }
        Err(error) => GroupingResult {
            fingerprint: fingerprint::raw_stack(
                input.language,
                input.error_kind,
                input.stack,
                *policy,
                policy_id,
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
    fn arbitrary_java_identifiers_use_raw_stack_fallback() {
        let input = |stack| GroupingInput {
            language: Language::Java,
            error_kind: "java.lang.RuntimeException",
            stack,
        };
        let first = group(input("alpha"));
        let second = group(input("beta"));

        assert_eq!(first.evidence, GroupingEvidence::RawStack);
        assert_ne!(first.fingerprint, second.fingerprint);
    }

    #[test]
    fn header_only_stack_reports_error_kind_evidence() {
        let result = group(GroupingInput {
            language: Language::Java,
            error_kind: "java.lang.RuntimeException",
            stack: "java.lang.RuntimeException: dynamic message",
        });

        assert_eq!(result.evidence, GroupingEvidence::ErrorKind);
    }

    #[test]
    fn policy_can_ignore_error_kind() {
        let input = |error_kind| GroupingInput {
            language: Language::JavaScript,
            error_kind,
            stack: "at load (/app.js:1:1)",
        };
        let policy = GroupingPolicy::default().with_error_kind(crate::ErrorKindPolicy::Ignore);

        assert_eq!(
            group_with_policy(input("TypeError"), &policy).fingerprint,
            group_with_policy(input("RangeError"), &policy).fingerprint
        );
    }

    #[test]
    fn policy_can_ignore_raw_stack_fallback() {
        let input = |stack| GroupingInput {
            language: Language::Java,
            error_kind: "Error",
            stack,
        };
        let policy = GroupingPolicy::default().with_raw_stack(crate::RawStackPolicy::ErrorKindOnly);

        assert_eq!(
            group_with_policy(input("first unsupported stack"), &policy).fingerprint,
            group_with_policy(input("second unsupported stack"), &policy).fingerprint
        );
    }

    #[test]
    fn parser_limits_are_part_of_grouping_policy() {
        let policy = GroupingPolicy::default().with_parser_limits(crate::ParserLimits {
            max_input_bytes: 4,
            ..crate::ParserLimits::default()
        });

        let result = group_with_policy(
            GroupingInput {
                language: Language::Java,
                error_kind: "Error",
                stack: "at a.B.run(B.java:1)",
            },
            &policy,
        );

        assert_eq!(result.evidence, GroupingEvidence::RawStack);
        assert!(matches!(
            result.parse_error,
            Some(ParseError::InputTooLarge { limit: 4, .. })
        ));
    }
}
