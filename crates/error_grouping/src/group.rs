use crate::fingerprint;
use std::{
    error::Error,
    fmt,
    sync::{Arc, LazyLock},
};

use crate::{Fingerprint, GroupingPolicy, Language, ParseError, ParseWarnings, RawStackPolicy};

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

/// Why grouping used only the authoritative error kind.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KindOnlyReason {
    EmptyStack,
    Policy,
    NoUsableFrames,
    ParseFailure(ParseError),
}

/// Structurally valid provenance for a grouping decision.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GroupingOutcome {
    Frames {
        contributing_frames: usize,
        warnings: ParseWarnings,
    },
    RawFallback {
        reason: ParseError,
    },
    KindOnly {
        reason: KindOnlyReason,
    },
}

/// Fingerprint plus enough provenance to observe degraded grouping.
#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use]
pub struct GroupingResult {
    /// Stable, versioned identity for storage and comparison.
    pub fingerprint: Fingerprint,
    /// Source material used to derive the fingerprint.
    pub outcome: GroupingOutcome,
}

impl GroupingResult {
    #[must_use]
    pub const fn parse_error(&self) -> Option<&ParseError> {
        match &self.outcome {
            GroupingOutcome::RawFallback { reason }
            | GroupingOutcome::KindOnly {
                reason: KindOnlyReason::ParseFailure(reason),
            } => Some(reason),
            _ => None,
        }
    }
}

/// A validated grouping policy compiled for repeated use.
#[derive(Clone, Debug)]
pub struct Grouper {
    policy: Arc<GroupingPolicy>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidPolicy(&'static str);

impl fmt::Display for InvalidPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl Error for InvalidPolicy {}

fn validate(valid: bool, message: &'static str) -> Result<(), InvalidPolicy> {
    valid.then_some(()).ok_or(InvalidPolicy(message))
}

impl Grouper {
    /// Validate an owned or shared policy for repeated use.
    pub fn new(policy: impl Into<Arc<GroupingPolicy>>) -> Result<Self, InvalidPolicy> {
        let policy = policy.into();
        let limits = policy.parser_limits;
        validate(
            limits.max_input_bytes > 0 && limits.max_lines > 0 && limits.max_line_bytes > 0,
            "parser limits must be greater than zero",
        )?;
        validate(
            !matches!(policy.raw_stack, RawStackPolicy::Bounded { max_bytes: 0 }),
            "raw-stack fallback must retain at least one byte",
        )?;
        validate(
            policy.frames.max_frames <= crate::parser::MAX_RETAINED_FRAMES,
            "max_frames exceeds the parser retention limit",
        )?;
        validate(
            policy.frames.fields.is_valid(),
            "frame fields contain unknown bits",
        )?;
        validate(
            policy
                .frames
                .exclusions
                .iter()
                .all(|rule| !rule.pattern().is_empty()),
            "frame-rule patterns must not be empty",
        )?;
        Ok(Self { policy })
    }

    #[must_use]
    pub fn policy(&self) -> &GroupingPolicy {
        &self.policy
    }

    pub fn group(&self, input: GroupingInput<'_>) -> GroupingResult {
        group_with_policy(input, &self.policy)
    }
}

/// Group an error using the default bounded policy.
///
/// Unrecognized non-empty stacks contribute bounded raw evidence rather than
/// collapsing all errors of the same kind into one low-confidence group.
pub fn group(input: GroupingInput<'_>) -> GroupingResult {
    static DEFAULT: LazyLock<Grouper> = LazyLock::new(|| {
        Grouper::new(GroupingPolicy::default()).expect("default grouping policy is valid")
    });
    DEFAULT.group(input)
}

fn group_with_policy(input: GroupingInput<'_>, policy: &GroupingPolicy) -> GroupingResult {
    match input
        .language
        .parse_stack_with_limits(input.stack, &policy.parser_limits)
    {
        Ok(trace) => {
            let (fingerprint, contributing_frames) =
                fingerprint::parsed(input.language, &trace, input.error_kind, policy);
            let outcome = if contributing_frames > 0 {
                GroupingOutcome::Frames {
                    contributing_frames,
                    warnings: trace.warnings,
                }
            } else {
                GroupingOutcome::KindOnly {
                    reason: if policy.segments == crate::SegmentSelection::ErrorKindOnly
                        || !policy.frames.includes_frames()
                    {
                        KindOnlyReason::Policy
                    } else {
                        KindOnlyReason::NoUsableFrames
                    },
                }
            };
            GroupingResult {
                fingerprint,
                outcome,
            }
        }
        Err(ParseError::Empty) => GroupingResult {
            fingerprint: fingerprint::kind_only(input.language, input.error_kind, policy),
            outcome: GroupingOutcome::KindOnly {
                reason: KindOnlyReason::EmptyStack,
            },
        },
        Err(error) => {
            let fingerprint =
                fingerprint::raw_stack(input.language, input.error_kind, input.stack, policy);
            let outcome = match policy.raw_stack {
                RawStackPolicy::Bounded { .. } => GroupingOutcome::RawFallback { reason: error },
                RawStackPolicy::ErrorKindOnly => GroupingOutcome::KindOnly {
                    reason: KindOnlyReason::ParseFailure(error),
                },
            };
            GroupingResult {
                fingerprint,
                outcome,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(language: Language, error_kind: &'a str, stack: &'a str) -> GroupingInput<'a> {
        GroupingInput {
            language,
            error_kind,
            stack,
        }
    }

    #[test]
    fn recognized_stack_reports_parsed_evidence() {
        let result = group(input(
            Language::JavaScript,
            "TypeError",
            "at render (/app/main.js:10:2)",
        ));
        assert!(matches!(result.outcome, GroupingOutcome::Frames { .. }));
    }

    #[test]
    fn empty_stack_reports_error_kind_evidence() {
        let result = group(input(Language::Java, "java.lang.RuntimeException", " \t"));
        assert!(matches!(result.outcome, GroupingOutcome::KindOnly { .. }));
    }

    #[test]
    fn unrecognized_stack_reports_raw_evidence_and_reason() {
        let result = group(input(
            Language::Java,
            "java.lang.RuntimeException",
            "not a stack",
        ));
        assert_eq!(result.parse_error(), Some(&ParseError::Unrecognized));
        assert!(matches!(
            result.outcome,
            GroupingOutcome::RawFallback { .. }
        ));
    }

    #[test]
    fn different_unrecognized_stacks_do_not_merge() {
        let first = group(input(
            Language::Java,
            "java.lang.RuntimeException",
            "first unsupported stack",
        ));
        let second = group(input(
            Language::Java,
            "java.lang.RuntimeException",
            "second unsupported stack",
        ));
        assert_ne!(first.fingerprint, second.fingerprint);
    }

    #[test]
    fn arbitrary_java_identifiers_use_raw_stack_fallback() {
        let first = group(input(Language::Java, "java.lang.RuntimeException", "alpha"));
        let second = group(input(Language::Java, "java.lang.RuntimeException", "beta"));

        assert!(matches!(first.outcome, GroupingOutcome::RawFallback { .. }));
        assert_ne!(first.fingerprint, second.fingerprint);
    }

    #[test]
    fn header_only_stack_reports_error_kind_evidence() {
        let result = group(input(
            Language::Java,
            "java.lang.RuntimeException",
            "java.lang.RuntimeException: dynamic message",
        ));

        assert!(matches!(result.outcome, GroupingOutcome::KindOnly { .. }));
    }

    #[test]
    fn policy_can_ignore_error_kind() {
        let policy = GroupingPolicy::default().include_error_kind(false);

        assert_eq!(
            group_with_policy(
                input(Language::JavaScript, "TypeError", "at load (/app.js:1:1)"),
                &policy,
            )
            .fingerprint,
            group_with_policy(
                input(Language::JavaScript, "RangeError", "at load (/app.js:1:1)"),
                &policy,
            )
            .fingerprint
        );
    }

    #[test]
    fn policy_can_ignore_raw_stack_fallback() {
        let policy = GroupingPolicy::default().with_raw_stack(crate::RawStackPolicy::ErrorKindOnly);

        assert_eq!(
            group_with_policy(
                input(Language::Java, "Error", "first unsupported stack"),
                &policy,
            )
            .fingerprint,
            group_with_policy(
                input(Language::Java, "Error", "second unsupported stack"),
                &policy,
            )
            .fingerprint
        );
    }

    #[test]
    fn ignored_raw_stack_reports_error_kind_evidence() {
        let policy = GroupingPolicy::default().with_raw_stack(crate::RawStackPolicy::ErrorKindOnly);
        let result =
            group_with_policy(input(Language::Java, "Error", "unsupported stack"), &policy);

        assert!(matches!(result.outcome, GroupingOutcome::KindOnly { .. }));
    }

    #[test]
    fn error_kind_only_segment_policy_reports_error_kind_evidence() {
        let policy =
            GroupingPolicy::default().with_segments(crate::SegmentSelection::ErrorKindOnly);
        let result = group_with_policy(
            input(Language::JavaScript, "TypeError", "at load (/app.js:1:1)"),
            &policy,
        );

        assert!(matches!(result.outcome, GroupingOutcome::KindOnly { .. }));
    }

    #[test]
    fn oversized_whitespace_respects_parser_limits() {
        let policy = GroupingPolicy::default().with_parser_limits(crate::ParserLimits {
            max_input_bytes: 4,
            ..crate::ParserLimits::default()
        });
        let result = group_with_policy(input(Language::Java, "Error", "     "), &policy);

        assert_eq!(
            result.parse_error(),
            Some(&ParseError::InputTooLarge {
                actual: 5,
                limit: 4,
            })
        );
    }

    #[test]
    fn parser_limits_are_part_of_grouping_policy() {
        let policy = GroupingPolicy::default().with_parser_limits(crate::ParserLimits {
            max_input_bytes: 4,
            ..crate::ParserLimits::default()
        });

        let result = group_with_policy(
            input(Language::Java, "Error", "at a.B.run(B.java:1)"),
            &policy,
        );

        assert!(matches!(
            result.outcome,
            GroupingOutcome::RawFallback { .. }
        ));
        assert!(matches!(
            result.parse_error(),
            Some(ParseError::InputTooLarge { limit: 4, .. })
        ));
    }

    #[test]
    fn grouper_rejects_invalid_configuration() {
        let empty_rule = GroupingPolicy::default().with_frames(
            crate::FramePolicy::default().with_exclusions(vec![crate::FrameRule::new(
                crate::FrameField::Function,
                crate::FrameMatcher::prefix(""),
            )]),
        );
        assert!(Grouper::new(empty_rule).is_err());
        assert!(
            Grouper::new(
                GroupingPolicy::default()
                    .with_raw_stack(crate::RawStackPolicy::Bounded { max_bytes: 0 })
            )
            .is_err()
        );
        assert!(
            Grouper::new(
                GroupingPolicy::default()
                    .with_frames(crate::FramePolicy::default().with_max_frames(257))
            )
            .is_err()
        );
    }

    #[test]
    fn frames_without_selected_identity_are_kind_only() {
        let policy = GroupingPolicy::default()
            .with_frames(crate::FramePolicy::default().with_fields(crate::FrameFields::FUNCTION));
        let result = group_with_policy(
            input(Language::JavaScript, "Error", "at /app.js:1:1"),
            &policy,
        );
        assert!(matches!(result.outcome, GroupingOutcome::KindOnly { .. }));
    }

    #[test]
    fn partial_and_truncated_parses_report_warnings() {
        let partial = group(input(
            Language::Java,
            "Error",
            "Error: bad\n at malformed\n at app.Main.run(Main.java:1)",
        ));
        assert!(matches!(
            partial.outcome,
            GroupingOutcome::Frames { warnings, .. }
                if warnings.malformed_frame
        ));

        let mut stack = String::from("Error: bad\n");
        for index in 0..257 {
            stack.push_str(&format!("at f{index} (/app.js:{index}:1)\n"));
        }
        let truncated = group(input(Language::JavaScript, "Error", &stack));
        assert!(matches!(
            truncated.outcome,
            GroupingOutcome::Frames { warnings, .. }
                if warnings.truncated
        ));
    }
}
