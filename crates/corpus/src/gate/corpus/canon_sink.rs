//! Shared byte writer for the canonical hashes used by the corpus and regress.
//!
//! These helpers are part of the frozen corpus hash contract. Keep their byte
//! encodings in one place so the archive walk and regress cannot drift apart.

use std::sync::Arc;

use xxhash_rust::xxh3::Xxh3;

use super::super::eliv::DetailAttr;

pub(crate) trait CanonSink {
    fn bytes(&mut self, bytes: &[u8]);
}

pub(crate) struct HashSink(Xxh3);

impl HashSink {
    pub(crate) fn new() -> Self {
        Self(Xxh3::new())
    }

    pub(crate) fn finish(self) -> u128 {
        self.0.digest128()
    }
}

impl CanonSink for HashSink {
    fn bytes(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
}

pub(crate) fn write_len(sink: &mut impl CanonSink, len: usize) {
    sink.bytes(&u64::try_from(len).unwrap_or(u64::MAX).to_le_bytes());
}

pub(crate) fn write_string(sink: &mut impl CanonSink, value: &str) {
    write_len(sink, value.len());
    sink.bytes(value.as_bytes());
}

pub(crate) fn write_detail_attr(sink: &mut impl CanonSink, value: &DetailAttr) {
    match value {
        DetailAttr::String(value) => {
            sink.bytes(&[1]);
            write_string(sink, value);
        }
        DetailAttr::Float(value) => {
            sink.bytes(&[2]);
            sink.bytes(&value.to_le_bytes());
        }
        DetailAttr::Double(value) => {
            sink.bytes(&[3]);
            sink.bytes(&value.to_le_bytes());
        }
        DetailAttr::Int(value) => {
            sink.bytes(&[4]);
            sink.bytes(&value.to_le_bytes());
        }
        DetailAttr::UInt(value) => {
            sink.bytes(&[5]);
            sink.bytes(&value.to_le_bytes());
        }
        DetailAttr::SInt(value) => {
            sink.bytes(&[6]);
            sink.bytes(&value.to_le_bytes());
        }
        DetailAttr::Bool(value) => sink.bytes(&[7, u8::from(*value)]),
    }
}

pub(crate) fn detail_attrs_hash(attrs: &[(Arc<str>, DetailAttr)]) -> u128 {
    let mut sink = HashSink::new();
    write_len(&mut sink, attrs.len());
    for (key, value) in attrs {
        write_string(&mut sink, key);
        write_detail_attr(&mut sink, value);
    }
    sink.finish()
}
