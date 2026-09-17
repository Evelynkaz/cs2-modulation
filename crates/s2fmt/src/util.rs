//! Bounds-checked little-endian cursor over a byte slice.

/// A read past the end of the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("read of {len} bytes at offset {offset} exceeds buffer of {size} bytes")]
pub struct OutOfBounds {
    pub offset: usize,
    pub len: usize,
    pub size: usize,
}

/// An error from reading a value out of a [`Reader`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReadError {
    #[error(transparent)]
    OutOfBounds(#[from] OutOfBounds),
    #[error("missing NUL terminator for C string at offset {offset}")]
    MissingNul { offset: usize },
    #[error("invalid UTF-8 in C string at offset {offset}")]
    InvalidUtf8 { offset: usize },
}

/// A bounds-checked, little-endian cursor over a byte slice.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn set_pos(&mut self, pos: usize) {
        self.pos = pos;
    }

    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    fn check(&self, len: usize) -> Result<(), OutOfBounds> {
        if self
            .pos
            .checked_add(len)
            .is_some_and(|end| end <= self.buf.len())
        {
            Ok(())
        } else {
            Err(OutOfBounds {
                offset: self.pos,
                len,
                size: self.buf.len(),
            })
        }
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], ReadError> {
        self.check(n)?;
        let slice = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    pub fn u8(&mut self) -> Result<u8, ReadError> {
        Ok(self.bytes(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, ReadError> {
        Ok(u16::from_le_bytes(self.bytes(2)?.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> Result<u32, ReadError> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    pub fn i32(&mut self) -> Result<i32, ReadError> {
        Ok(i32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Result<u64, ReadError> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }

    pub fn i64(&mut self) -> Result<i64, ReadError> {
        Ok(i64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }

    #[cfg(test)]
    pub fn f32(&mut self) -> Result<f32, ReadError> {
        Ok(f32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    pub fn f64(&mut self) -> Result<f64, ReadError> {
        Ok(f64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }

    /// Reads a NUL-terminated UTF-8 string, advancing past the NUL.
    pub fn cstr(&mut self) -> Result<&'a str, ReadError> {
        let start = self.pos;
        let rest = self.buf.get(start..).ok_or(OutOfBounds {
            offset: start,
            len: 1,
            size: self.buf.len(),
        })?;
        let nul = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or(ReadError::MissingNul { offset: start })?;
        let bytes = &self.buf[start..start + nul];
        self.pos = start + nul + 1;
        std::str::from_utf8(bytes).map_err(|_| ReadError::InvalidUtf8 { offset: start })
    }

    /// Advances `pos` to the next multiple of `n` relative to the start of the buffer.
    #[cfg(test)]
    pub fn align(&mut self, n: usize) -> Result<(), ReadError> {
        if n == 0 {
            return Ok(());
        }
        let aligned = self.pos.div_ceil(n).checked_mul(n).ok_or(OutOfBounds {
            offset: self.pos,
            len: 1,
            size: self.buf.len(),
        })?;
        self.check(aligned - self.pos)?;
        self.pos = aligned;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_u8() {
        let mut r = Reader::new(&[0x42]);
        assert_eq!(r.u8().unwrap(), 0x42);
        assert_eq!(r.pos(), 1);
    }

    #[test]
    fn reads_u16() {
        let mut r = Reader::new(&[0x01, 0x02]);
        assert_eq!(r.u16().unwrap(), 0x0201);
    }

    #[test]
    fn reads_u32() {
        let mut r = Reader::new(&[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(r.u32().unwrap(), 0x0403_0201);
    }

    #[test]
    fn reads_i32() {
        let buf = (-1i32).to_le_bytes();
        let mut r = Reader::new(&buf);
        assert_eq!(r.i32().unwrap(), -1);
    }

    #[test]
    fn reads_u64() {
        let buf = 42u64.to_le_bytes();
        let mut r = Reader::new(&buf);
        assert_eq!(r.u64().unwrap(), 42);
    }

    #[test]
    fn reads_i64() {
        let buf = (-42i64).to_le_bytes();
        let mut r = Reader::new(&buf);
        assert_eq!(r.i64().unwrap(), -42);
    }

    #[test]
    fn reads_f32() {
        let buf = 1.5f32.to_le_bytes();
        let mut r = Reader::new(&buf);
        assert_eq!(r.f32().unwrap(), 1.5);
    }

    #[test]
    fn reads_f64() {
        let buf = 2.5f64.to_le_bytes();
        let mut r = Reader::new(&buf);
        assert_eq!(r.f64().unwrap(), 2.5);
    }

    #[test]
    fn reads_bytes() {
        let mut r = Reader::new(&[1, 2, 3, 4]);
        assert_eq!(r.bytes(3).unwrap(), &[1, 2, 3]);
        assert_eq!(r.pos(), 3);
    }

    #[test]
    fn reads_cstr() {
        let mut r = Reader::new(b"hello\0world");
        assert_eq!(r.cstr().unwrap(), "hello");
        assert_eq!(r.pos(), 6);
    }

    #[test]
    fn cstr_missing_nul() {
        let mut r = Reader::new(b"hello");
        assert_eq!(r.cstr().unwrap_err(), ReadError::MissingNul { offset: 0 });
    }

    #[test]
    fn cstr_invalid_utf8() {
        let mut r = Reader::new(&[0xff, 0xfe, 0]);
        assert_eq!(r.cstr().unwrap_err(), ReadError::InvalidUtf8 { offset: 0 });
    }

    #[test]
    fn aligns_position() {
        let mut r = Reader::new(&[0u8; 8]);
        r.set_pos(1);
        r.align(4).unwrap();
        assert_eq!(r.pos(), 4);
    }

    #[test]
    fn align_already_aligned() {
        let mut r = Reader::new(&[0u8; 8]);
        r.set_pos(4);
        r.align(4).unwrap();
        assert_eq!(r.pos(), 4);
    }

    #[test]
    fn align_out_of_bounds() {
        let mut r = Reader::new(&[0u8; 3]);
        r.set_pos(2);
        assert_eq!(
            r.align(4).unwrap_err(),
            ReadError::OutOfBounds(OutOfBounds {
                offset: 2,
                len: 2,
                size: 3
            })
        );
    }

    #[test]
    fn remaining_and_len() {
        let mut r = Reader::new(&[0u8; 10]);
        assert_eq!(r.len(), 10);
        assert_eq!(r.remaining(), 10);
        r.set_pos(4);
        assert_eq!(r.remaining(), 6);
    }

    #[test]
    fn out_of_bounds_u32() {
        let mut r = Reader::new(&[1, 2]);
        assert_eq!(
            r.u32().unwrap_err(),
            ReadError::OutOfBounds(OutOfBounds {
                offset: 0,
                len: 4,
                size: 2
            })
        );
    }

    #[test]
    fn out_of_bounds_bytes() {
        let mut r = Reader::new(&[1, 2]);
        assert_eq!(
            r.bytes(5).unwrap_err(),
            ReadError::OutOfBounds(OutOfBounds {
                offset: 0,
                len: 5,
                size: 2
            })
        );
    }

    #[test]
    fn cstr_past_end_no_panic() {
        let mut r = Reader::new(b"hello");
        r.set_pos(100);
        assert_eq!(
            r.cstr().unwrap_err(),
            ReadError::OutOfBounds(OutOfBounds {
                offset: 100,
                len: 1,
                size: 5
            })
        );
    }

    #[test]
    fn align_zero_is_noop() {
        let mut r = Reader::new(&[0u8; 8]);
        r.set_pos(3);
        r.align(0).unwrap();
        assert_eq!(r.pos(), 3);
    }

    #[test]
    fn align_overflow_no_panic() {
        let mut r = Reader::new(&[0u8; 8]);
        r.set_pos(usize::MAX - 1);
        assert_eq!(
            r.align(4).unwrap_err(),
            ReadError::OutOfBounds(OutOfBounds {
                offset: usize::MAX - 1,
                len: 1,
                size: 8
            })
        );
    }
}
