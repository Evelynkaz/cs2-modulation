//! Bounds-checked little-endian cursor over a byte slice, for the binary VBIB block
//! (`docs/FORMATS.md`-style parsing, matching `s2fmt::util::Reader`'s approach). Kept local
//! since `s2fmt`'s own `Reader` is crate-private.

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("read of {len} bytes at offset {offset} exceeds buffer of {size} bytes")]
pub struct OutOfBounds {
    pub offset: usize,
    pub len: usize,
    pub size: usize,
}

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

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], OutOfBounds> {
        self.check(n)?;
        let slice = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    pub fn u32(&mut self) -> Result<u32, OutOfBounds> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    pub fn i32(&mut self) -> Result<i32, OutOfBounds> {
        Ok(i32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    /// Reads a fixed `n`-byte slot holding a NUL-terminated string (the rest is padding),
    /// leniently: non-UTF-8 bytes are replaced rather than treated as a parse failure, matching
    /// `s2fmt::vpk::read_tree_cstr`'s tolerance.
    pub fn fixed_cstr(&mut self, n: usize) -> Result<String, OutOfBounds> {
        let slot = self.bytes(n)?;
        let end = slot.iter().position(|&b| b == 0).unwrap_or(slot.len());
        Ok(String::from_utf8_lossy(&slot[..end]).into_owned())
    }
}
