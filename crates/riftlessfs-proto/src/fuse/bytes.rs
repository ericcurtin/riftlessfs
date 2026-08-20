//! Tiny little-endian cursor reader/writer, used to keep the many
//! FUSE wire structs in `wire.rs` from turning into a sea of hand-counted
//! byte offsets.

use crate::error::{ProtoError, ProtoResult};

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> ProtoResult<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(ProtoError::Truncated)?;
        let s = self.buf.get(self.pos..end).ok_or(ProtoError::Truncated)?;
        self.pos = end;
        Ok(s)
    }

    pub fn u16(&mut self) -> ProtoResult<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> ProtoResult<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn i32(&mut self) -> ProtoResult<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> ProtoResult<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn skip(&mut self, n: usize) -> ProtoResult<()> {
        self.take(n)?;
        Ok(())
    }

    /// Everything not yet consumed.
    pub fn remaining(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }

    /// Read a single NUL-terminated string, returning it *without* the
    /// NUL, and leaving the cursor positioned just after it.
    pub fn cstr(&mut self) -> ProtoResult<&'a [u8]> {
        let rest = &self.buf[self.pos..];
        let nul = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or(ProtoError::Truncated)?;
        let s = &rest[..nul];
        self.pos += nul + 1;
        Ok(s)
    }
}

#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Writer { buf: Vec::new() }
    }

    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self
    }

    /// Append `s` followed by a NUL terminator.
    pub fn cstr(&mut self, s: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(s);
        self.buf.push(0);
        self
    }

    /// Zero-pad up to `len` bytes total (no-op if already that long or
    /// longer).
    pub fn pad_to(&mut self, len: usize) -> &mut Self {
        if self.buf.len() < len {
            self.buf.resize(len, 0);
        }
        self
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_writer_roundtrip() {
        let mut w = Writer::new();
        w.u32(1).u64(2).u16(3).cstr(b"hi");
        let bytes = w.into_vec();

        let mut r = Reader::new(&bytes);
        assert_eq!(r.u32().unwrap(), 1);
        assert_eq!(r.u64().unwrap(), 2);
        assert_eq!(r.u16().unwrap(), 3);
        assert_eq!(r.cstr().unwrap(), b"hi");
    }

    #[test]
    fn reader_reports_truncation_instead_of_panicking() {
        let bytes = [1u8, 2, 3];
        let mut r = Reader::new(&bytes);
        assert!(matches!(r.u32(), Err(ProtoError::Truncated)));
    }

    #[test]
    fn pad_to_is_a_noop_if_already_long_enough() {
        let mut w = Writer::new();
        w.bytes(&[1, 2, 3, 4]);
        w.pad_to(2);
        assert_eq!(w.into_vec(), vec![1, 2, 3, 4]);
    }
}
