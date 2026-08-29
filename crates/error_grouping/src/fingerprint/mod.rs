//! Noise-resistant error fingerprints.

mod canonical;
mod normalize;
mod options;

use std::fmt;

use canonical::{Canonical, Tag};
use normalize::{FrameIdentity, frame_identity, is_runtime_frame, normalized_kind};
pub(crate) use options::FingerprintOptions;

use crate::Language;
use crate::ast::{SegmentRelation, StackTrace, TraceSegment};

const DOMAIN: &[u8] = b"error-grouping/fingerprint/v1";
/// Prefix identifying the canonical fingerprint format.
pub const FINGERPRINT_VERSION: &str = "eg1";

/// A stable SHA-256 error-group identity.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    /// Return the raw SHA-256 digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Encode the digest as lowercase hexadecimal without the version prefix.
    #[must_use]
    pub fn to_hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut output = String::with_capacity(64);
        for byte in self.0 {
            output.push(char::from(HEX[usize::from(byte >> 4)]));
            output.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        output
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        const PREFIX_LEN: usize = FINGERPRINT_VERSION.len() + 1;
        let mut output = [0_u8; PREFIX_LEN + 64];
        output[..FINGERPRINT_VERSION.len()].copy_from_slice(FINGERPRINT_VERSION.as_bytes());
        output[FINGERPRINT_VERSION.len()] = b'_';
        for (index, byte) in self.0.iter().copied().enumerate() {
            output[PREFIX_LEN + index * 2] = HEX[usize::from(byte >> 4)];
            output[PREFIX_LEN + index * 2 + 1] = HEX[usize::from(byte & 0x0f)];
        }
        let output = str::from_utf8(&output).map_err(|_| fmt::Error)?;
        f.write_str(output)
    }
}

pub(super) fn parsed(
    language: Language,
    trace: &StackTrace<'_>,
    authoritative_kind: &str,
    options: FingerprintOptions<'_>,
) -> Fingerprint {
    let mut canonical = header(language, authoritative_kind, root_kind(trace), options);

    if options.max_segments > 0
        && let Some(root) = trace.segments.first()
    {
        write_segment(&mut canonical, language, root, false, options);

        if options.max_segments > 1
            && let Some(cause) = terminal_cause(trace)
        {
            write_segment(&mut canonical, language, cause, true, options);
        }
    } else {
        write_empty_root(&mut canonical);
    }

    finish(canonical)
}

pub(super) fn kind_only(
    language: Language,
    error_kind: &str,
    options: FingerprintOptions<'_>,
) -> Fingerprint {
    let mut canonical = header(language, error_kind, None, options);
    write_empty_root(&mut canonical);
    finish(canonical)
}

pub(super) fn raw_stack(
    language: Language,
    error_kind: &str,
    stack: &str,
    options: FingerprintOptions<'_>,
) -> Fingerprint {
    let mut canonical = header(language, error_kind, None, options);
    if options.include_raw_stack {
        canonical.tag(Tag::RawStack);
        canonical.field(stack.as_bytes());
    } else {
        write_empty_root(&mut canonical);
    }
    finish(canonical)
}

fn header(
    language: Language,
    authoritative_kind: &str,
    parsed_kind: Option<&str>,
    options: FingerprintOptions<'_>,
) -> Canonical {
    let mut canonical = Canonical::default();
    canonical.field(DOMAIN);
    canonical.byte(language_tag(language));
    let authoritative_kind = (!authoritative_kind.trim().is_empty()).then_some(authoritative_kind);
    let kind = options
        .include_error_kind
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
    options: FingerprintOptions<'_>,
) {
    canonical.tag(Tag::Segment);
    canonical.byte(relation_tag(segment.relation));
    let kind = (include_kind && options.include_error_kind)
        .then(|| normalized_kind(language, segment.error_kind))
        .flatten();
    canonical.optional_text(kind.as_deref());

    let frames = options.frames;
    if !frames.includes_frames() {
        canonical.tag(Tag::EndSegment);
        return;
    }

    let filter_runtime = frames.filter_runtime
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
        if frames.deduplicate_adjacent
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

fn finish(mut canonical: Canonical) -> Fingerprint {
    canonical.tag(Tag::End);
    Fingerprint(canonical.finish())
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
