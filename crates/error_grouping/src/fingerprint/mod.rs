//! Noise-resistant error fingerprints.

mod canonical;
mod normalize;
mod options;

use std::fmt;

use canonical::{Canonical, Tag};
use normalize::{FrameIdentity, frame_identity, is_runtime_frame, normalized_kind};
pub use options::{
    FrameField, FrameFields, FrameMatcher, FramePolicy, FrameRule, GroupingPolicy, RawStackPolicy,
    SegmentSelection,
};

use crate::Language;
use crate::ast::{SegmentRelation, StackTrace, TraceSegment};

const DOMAIN: &[u8] = b"error-grouping/fingerprint/v1";
/// Prefix identifying the canonical fingerprint format.
pub const FINGERPRINT_VERSION: &str = "eg1";

/// A stable SHA-256 error-group identity.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Fingerprint {
    digest: [u8; 32],
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{FINGERPRINT_VERSION}_")?;
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut output = [0_u8; 64];
        for (byte, encoded) in self.digest.iter().zip(output.as_chunks_mut::<2>().0) {
            encoded[0] = HEX[usize::from(byte >> 4)];
            encoded[1] = HEX[usize::from(byte & 0x0f)];
        }
        f.write_str(str::from_utf8(&output).map_err(|_| fmt::Error)?)
    }
}

pub(super) fn parsed(
    language: Language,
    trace: &StackTrace<'_>,
    authoritative_kind: &str,
    policy: &GroupingPolicy,
) -> (Fingerprint, usize) {
    let terminal_frames = policy.segments == SegmentSelection::TerminalCauseFrames;
    let authoritative_kind = nonempty(authoritative_kind);
    let header_kind = if terminal_frames {
        None
    } else {
        authoritative_kind.or_else(|| root_kind(trace))
    };
    let mut canonical = header(language, header_kind, policy);
    let mut contributing_frames = 0;

    match policy.segments {
        SegmentSelection::ErrorKindOnly => write_empty_root(&mut canonical),
        SegmentSelection::Root | SegmentSelection::RootAndTerminalCause => {
            if let Some(root) = trace.segments.first() {
                contributing_frames +=
                    write_segment(&mut canonical, language, root, root.relation, None, policy);
                if policy.segments == SegmentSelection::RootAndTerminalCause
                    && let Some(cause) = terminal_cause(trace)
                {
                    contributing_frames += write_segment(
                        &mut canonical,
                        language,
                        cause,
                        cause.relation,
                        cause.error_kind,
                        policy,
                    );
                }
            } else {
                write_empty_root(&mut canonical);
            }
        }
        SegmentSelection::TerminalCauseFrames => {
            if let Some(segment) = terminal_cause(trace).or_else(|| trace.segments.first()) {
                let kind = if segment.frames.is_empty() {
                    segment.error_kind.or(authoritative_kind)
                } else {
                    None
                };
                contributing_frames += write_segment(
                    &mut canonical,
                    language,
                    segment,
                    SegmentRelation::Root,
                    kind,
                    policy,
                );
            } else {
                write_empty_root(&mut canonical);
            }
        }
    }

    (finish(canonical), contributing_frames)
}

pub(super) fn kind_only(
    language: Language,
    error_kind: &str,
    policy: &GroupingPolicy,
) -> Fingerprint {
    let mut canonical = header(language, nonempty(error_kind), policy);
    write_empty_root(&mut canonical);
    finish(canonical)
}

pub(super) fn raw_stack(
    language: Language,
    error_kind: &str,
    stack: &str,
    policy: &GroupingPolicy,
) -> Fingerprint {
    let mut canonical = header(language, nonempty(error_kind), policy);
    match policy.raw_stack {
        RawStackPolicy::Bounded { max_bytes } => {
            write_bounded_raw_stack(&mut canonical, stack, max_bytes);
        }
        RawStackPolicy::ErrorKindOnly => {
            write_empty_root(&mut canonical);
        }
    }
    finish(canonical)
}

fn header(language: Language, kind: Option<&str>, policy: &GroupingPolicy) -> Canonical {
    let mut canonical = Canonical::default();
    canonical.field(DOMAIN);
    canonical.byte(language_tag(language));
    let kind = if policy.include_error_kind {
        normalized_kind(language, kind)
    } else {
        None
    };
    canonical.optional_text(kind.as_deref());
    canonical
}

fn nonempty(value: &str) -> Option<&str> {
    (!value.trim().is_empty()).then_some(value)
}

fn root_kind<'a>(trace: &StackTrace<'a>) -> Option<&'a str> {
    trace
        .segments
        .first()
        .and_then(|segment| segment.error_kind)
}

fn terminal_cause<'s, 'a>(trace: &'s StackTrace<'a>) -> Option<&'s TraceSegment<'a>> {
    trace
        .segments
        .iter()
        .rev()
        .filter(|segment| {
            matches!(
                segment.relation,
                SegmentRelation::Cause | SegmentRelation::Context
            )
        })
        .min_by_key(|segment| segment.depth)
}

fn write_empty_root(canonical: &mut Canonical) {
    canonical.tag(Tag::Segment);
    canonical.byte(relation_tag(SegmentRelation::Root));
    canonical.optional_text(None);
    canonical.tag(Tag::EndSegment);
}

fn write_segment(
    canonical: &mut Canonical,
    language: Language,
    segment: &TraceSegment<'_>,
    relation: SegmentRelation,
    kind: Option<&str>,
    policy: &GroupingPolicy,
) -> usize {
    canonical.tag(Tag::Segment);
    canonical.byte(relation_tag(relation));
    let kind = if policy.include_error_kind {
        normalized_kind(language, kind)
    } else {
        None
    };
    canonical.optional_text(kind.as_deref());

    let frames = &policy.frames;
    if !frames.includes_frames() {
        canonical.tag(Tag::EndSegment);
        return 0;
    }

    let filter_runtime = !frames.include_runtime_frames
        && segment
            .frames
            .iter()
            .any(|frame| !is_runtime_frame(language, frame));
    let mut previous: Option<FrameIdentity<'_>> = None;
    let mut included = 0;

    for frame in &segment.frames {
        if included >= frames.max_frames {
            break;
        }
        if filter_runtime && is_runtime_frame(language, frame) {
            continue;
        }
        let identity = frame_identity(language, frame);
        if frames.excludes(&identity) {
            continue;
        }
        let values = frames.fields.values(&identity);
        if values.iter().all(Option::is_none) {
            continue;
        }
        if frames.deduplicate_adjacent_frames
            && previous
                .as_ref()
                .is_some_and(|previous| frames.same_identity(previous, &identity))
        {
            continue;
        }

        canonical.tag(Tag::Frame);
        for field in values {
            canonical.optional_text(field);
        }
        previous = Some(identity);
        included += 1;
    }
    canonical.tag(Tag::EndSegment);
    included
}

fn write_bounded_raw_stack(canonical: &mut Canonical, stack: &str, max_bytes: usize) {
    canonical.tag(Tag::RawStack);
    canonical.field(&(stack.len() as u64).to_be_bytes());
    if stack.len() <= max_bytes {
        canonical.field(stack.as_bytes());
        return;
    }

    let start_budget = max_bytes / 2;
    let end_budget = max_bytes - start_budget;
    let start_end = floor_char_boundary(stack, start_budget);
    let end_start = ceil_char_boundary(stack, stack.len().saturating_sub(end_budget));
    canonical.field(&stack.as_bytes()[..start_end]);
    canonical.field(&stack.as_bytes()[end_start..]);
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn finish(mut canonical: Canonical) -> Fingerprint {
    canonical.tag(Tag::End);
    Fingerprint {
        digest: canonical.finish(),
    }
}

const fn language_tag(language: Language) -> u8 {
    match language {
        Language::Java => 1,
        Language::Rust => 2,
        Language::JavaScript => 3,
        Language::Python => 4,
        Language::Php => 5,
        Language::Go => 6,
        Language::Swift => 7,
    }
}

const fn relation_tag(relation: SegmentRelation) -> u8 {
    match relation {
        SegmentRelation::Root => 0,
        SegmentRelation::Cause => 1,
        SegmentRelation::Context => 2,
        SegmentRelation::Suppressed => 3,
    }
}

#[cfg(test)]
mod tests;
