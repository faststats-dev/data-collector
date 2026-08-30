//! Noise-resistant error fingerprints.

mod canonical;
mod normalize;
mod options;

use std::fmt;

use canonical::{Canonical, Tag};
use normalize::{FrameIdentity, frame_identity, is_runtime_frame, normalized_kind};
pub use options::{
    AdjacentFramePolicy, ErrorKindPolicy, FrameExclusion, FrameField, FrameFields, FrameMatcher,
    FramePolicy, GroupingPolicy, GroupingPolicyId, RawStackPolicy, RuntimeFramePolicy,
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
    policy: GroupingPolicyId,
    digest: [u8; 32],
}

impl Fingerprint {
    /// Return the raw SHA-256 digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.digest
    }

    /// Return the identity of the exact policy that produced this fingerprint.
    #[must_use]
    pub const fn policy(&self) -> GroupingPolicyId {
        self.policy
    }

    /// Encode only the SHA-256 digest as lowercase hexadecimal.
    ///
    /// This omits the policy identity and should not be used as a stored group ID.
    #[must_use]
    pub fn digest_hex(self) -> String {
        Hex(&self.digest).to_string()
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

impl fmt::Display for GroupingPolicyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Hex(&self.0).fmt(f)
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{FINGERPRINT_VERSION}_{}_{digest}",
            self.policy,
            digest = Hex(&self.digest)
        )
    }
}

struct Hex<'a>(&'a [u8]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut output = [0_u8; 64];
        for (byte, encoded) in self.0.iter().zip(output.chunks_exact_mut(2)) {
            encoded[0] = HEX[usize::from(byte >> 4)];
            encoded[1] = HEX[usize::from(byte & 0x0f)];
        }
        let output = &output[..self.0.len() * 2];
        f.write_str(str::from_utf8(output).map_err(|_| fmt::Error)?)
    }
}

pub(super) fn parsed(
    language: Language,
    trace: &StackTrace<'_>,
    authoritative_kind: &str,
    policy: GroupingPolicy<'_>,
    policy_id: GroupingPolicyId,
) -> Fingerprint {
    let mut canonical = header(language, authoritative_kind, root_kind(trace), policy);

    match policy.segments {
        SegmentSelection::ErrorKindOnly => write_empty_root(&mut canonical),
        SegmentSelection::Root | SegmentSelection::RootAndTerminalCause => {
            if let Some(root) = trace.segments.first() {
                write_segment(&mut canonical, language, root, false, policy);
                if policy.segments == SegmentSelection::RootAndTerminalCause
                    && let Some(cause) = terminal_cause(trace)
                {
                    write_segment(&mut canonical, language, cause, true, policy);
                }
            } else {
                write_empty_root(&mut canonical);
            }
        }
    }

    finish(canonical, policy_id)
}

pub(super) fn kind_only(
    language: Language,
    error_kind: &str,
    policy: GroupingPolicy<'_>,
    policy_id: GroupingPolicyId,
) -> Fingerprint {
    let mut canonical = header(language, error_kind, None, policy);
    write_empty_root(&mut canonical);
    finish(canonical, policy_id)
}

pub(super) fn raw_stack(
    language: Language,
    error_kind: &str,
    stack: &str,
    policy: GroupingPolicy<'_>,
    policy_id: GroupingPolicyId,
) -> Fingerprint {
    let mut canonical = header(language, error_kind, None, policy);
    match policy.raw_stack {
        RawStackPolicy::Bounded { max_bytes } => {
            write_bounded_raw_stack(&mut canonical, stack, max_bytes)
        }
        RawStackPolicy::ErrorKindOnly => write_empty_root(&mut canonical),
    }
    finish(canonical, policy_id)
}

fn header(
    language: Language,
    authoritative_kind: &str,
    parsed_kind: Option<&str>,
    policy: GroupingPolicy<'_>,
) -> Canonical {
    let mut canonical = Canonical::default();
    canonical.field(DOMAIN);
    canonical.byte(language_tag(language));
    let authoritative_kind = (!authoritative_kind.trim().is_empty()).then_some(authoritative_kind);
    let kind = (policy.error_kind == ErrorKindPolicy::Include)
        .then(|| normalized_kind(language, authoritative_kind.or(parsed_kind)))
        .flatten();
    canonical.optional_text(kind.as_deref());
    canonical
}

fn root_kind<'a>(trace: &'a StackTrace<'a>) -> Option<&'a str> {
    trace
        .segments
        .first()
        .and_then(|segment| segment.error_kind)
}

fn terminal_cause<'a>(trace: &'a StackTrace<'a>) -> Option<&'a TraceSegment<'a>> {
    trace.segments.iter().rev().find(|segment| {
        matches!(
            segment.relation,
            SegmentRelation::Cause | SegmentRelation::Context
        )
    })
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
    include_kind: bool,
    policy: GroupingPolicy<'_>,
) {
    canonical.tag(Tag::Segment);
    canonical.byte(relation_tag(segment.relation));
    let kind = (include_kind && policy.error_kind == ErrorKindPolicy::Include)
        .then(|| normalized_kind(language, segment.error_kind))
        .flatten();
    canonical.optional_text(kind.as_deref());

    let frames = policy.frames;
    if !frames.includes_frames() {
        canonical.tag(Tag::EndSegment);
        return;
    }

    let filter_runtime = frames.runtime_frames
        == RuntimeFramePolicy::ExcludeWhenApplicationFrameExists
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
        if frames.adjacent_frames == AdjacentFramePolicy::Deduplicate
            && previous
                .as_ref()
                .is_some_and(|previous| frames.same_identity(previous, &identity))
        {
            continue;
        }

        canonical.tag(Tag::Frame);
        for field in frames.fields.values(&identity) {
            canonical.optional_text(field);
        }
        previous = Some(identity);
        included += 1;
    }
    canonical.tag(Tag::EndSegment);
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

fn finish(mut canonical: Canonical, policy: GroupingPolicyId) -> Fingerprint {
    canonical.tag(Tag::End);
    Fingerprint {
        policy,
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
