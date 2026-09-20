//! Minimal streaming Matroska/MJPEG muxer. No seek tables, files, or JPEG copies.
//! Element IDs and codec mapping: https://www.matroska.org/technical/elements.html
//! Each changed image carries its original output-frame timestamp. FFmpeg's fps
//! filter expands the gaps by sharing decoded frames rather than decoding copies.

fn size(out: &mut Vec<u8>, value: usize) {
    let value = value as u64;
    let bytes = (1..=8)
        .find(|n| value < (1_u64 << (7 * n)) - 1)
        .expect("element fits EBML size");
    let encoded = (1_u64 << (7 * bytes)) | value;
    out.extend_from_slice(&encoded.to_be_bytes()[8 - bytes..]);
}
fn id(out: &mut Vec<u8>, value: u32) {
    let bytes = value.to_be_bytes();
    let start = (value.leading_zeros() / 8) as usize;
    out.extend_from_slice(&bytes[start..]);
}
fn tag(out: &mut Vec<u8>, key: u32, data: &[u8]) {
    id(out, key);
    size(out, data.len());
    out.extend_from_slice(data);
}
fn uint(out: &mut Vec<u8>, key: u32, value: u64) {
    let start = ((value.leading_zeros() / 8) as usize).min(7);
    tag(out, key, &value.to_be_bytes()[start..]);
}

pub(crate) fn header(width: u32, height: u32, fps: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    let mut ebml = Vec::new();
    for (key, value) in [(0x4286, 1), (0x42f7, 1), (0x42f2, 4), (0x42f3, 8)] {
        uint(&mut ebml, key, value);
    }
    tag(&mut ebml, 0x4282, b"matroska");
    uint(&mut ebml, 0x4287, 4);
    uint(&mut ebml, 0x4285, 2);
    tag(&mut out, 0x1a45dfa3, &ebml);
    // Segment of unknown length; terminated by EOF (valid streaming Matroska).
    out.extend_from_slice(&[
        0x18, 0x53, 0x80, 0x67, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    ]);
    let mut info = Vec::new();
    uint(&mut info, 0x2ad7b1, 1000); // TimestampScale: one microsecond.
    tag(&mut info, 0x4d80, b"rrweb2video");
    tag(&mut info, 0x5741, b"rrweb2video");
    tag(&mut out, 0x1549a966, &info);
    let mut track = Vec::new();
    uint(&mut track, 0xd7, 1);
    uint(&mut track, 0x73c5, 1);
    uint(&mut track, 0x83, 1);
    tag(&mut track, 0x86, b"V_MJPEG");
    uint(
        &mut track,
        0x23e383,
        (1_000_000_000_u64 + fps as u64 / 2) / fps as u64,
    );
    let mut video = Vec::new();
    uint(&mut video, 0xb0, width as u64);
    uint(&mut video, 0xba, height as u64);
    tag(&mut track, 0xe0, &video);
    let mut tracks = Vec::new();
    tag(&mut tracks, 0xae, &track);
    tag(&mut out, 0x1654ae6b, &tracks);
    out
}

/// Prefix for a Cluster containing one keyframe. The JPEG bytes follow directly.
pub(crate) fn frame_prefix(index: u64, fps: u32, jpeg_bytes: usize) -> Vec<u8> {
    let mut cluster = Vec::with_capacity(32);
    uint(
        &mut cluster,
        0xe7,
        (index * 1_000_000 + fps as u64 / 2) / fps as u64,
    );
    id(&mut cluster, 0xa3);
    size(&mut cluster, jpeg_bytes + 4);
    cluster.extend_from_slice(&[0x81, 0, 0, 0x80]); // Track 1, relative timestamp 0, keyframe.
    let mut prefix = Vec::with_capacity(48);
    id(&mut prefix, 0x1f43b675);
    size(&mut prefix, cluster.len() + jpeg_bytes);
    prefix.extend_from_slice(&cluster);
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ebml_size_boundaries_reserve_all_ones_for_unknown_length() {
        for (value, expected) in [
            (0, vec![0x80]),
            (126, vec![0xfe]),
            (127, vec![0x40, 0x7f]),
            (16382, vec![0x7f, 0xfe]),
            (16383, vec![0x20, 0x3f, 0xff]),
        ] {
            let mut bytes = Vec::new();
            size(&mut bytes, value);
            assert_eq!(bytes, expected);
        }
    }
    #[test]
    fn packet_keeps_original_frame_timestamp_and_declares_jpeg_length() {
        assert_eq!(
            frame_prefix(0, 10, 3),
            [
                0x1f, 0x43, 0xb6, 0x75, 0x8c, 0xe7, 0x81, 0, 0xa3, 0x87, 0x81, 0, 0, 0x80
            ]
        );
        let later = frame_prefix(394, 10, 3);
        assert!(
            later
                .windows(4)
                .any(|bytes| bytes == 39_400_000_u32.to_be_bytes())
        );
    }
}
