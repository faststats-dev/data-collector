use sha2::{Digest, Sha256};

#[repr(u8)]
pub(super) enum Tag {
    Segment = 0x10,
    Frame = 0x20,
    EndSegment = 0x2f,
    RawStack = 0x40,
    End = 0xff,
}

#[derive(Default)]
pub(super) struct Canonical(Sha256);

impl Canonical {
    pub(super) fn tag(&mut self, tag: Tag) {
        self.byte(tag as u8);
    }

    pub(super) fn byte(&mut self, value: u8) {
        self.0.update([value]);
    }

    pub(super) fn field(&mut self, value: &[u8]) {
        self.0.update((value.len() as u64).to_be_bytes());
        self.0.update(value);
    }

    pub(super) fn optional_text(&mut self, value: Option<&str>) {
        match value {
            Some(value) => {
                self.byte(1);
                self.field(value.as_bytes());
            }
            None => self.byte(0),
        }
    }

    pub(super) fn finish(self) -> [u8; 32] {
        self.0.finalize().into()
    }
}
