//! Bounds-checked little-endian cursor, plus the Source 2 "offset relative
//! to this field's own position" convention used throughout the vtex header
//! and the legacy REDI block (Texture.cs:420-539, `StreamHelpers.cs`'s
//! `ReadOffsetString`). Kept crate-private: `s2fmt::util::Reader` is
//! `pub(crate)` to that crate, so header/REDI parsing here needs its own.

use crate::error::TexError;

/// A read past the end of the buffer, or a string with no NUL terminator
/// within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("read of {len} bytes at offset {offset} exceeds buffer of {size} bytes")]
pub(crate) struct OutOfBounds {
    pub offset: usize,
    pub len: usize,
    pub size: usize,
}

impl From<OutOfBounds> for TexError {
    fn from(e: OutOfBounds) -> TexError {
        TexError::Truncated {
            detail: e.to_string(),
        }
    }
}

pub(crate) struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Cursor { buf, pos: 0 }
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

    pub fn u8(&mut self) -> Result<u8, OutOfBounds> {
        Ok(self.bytes(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, OutOfBounds> {
        Ok(u16::from_le_bytes(self.bytes(2)?.try_into().unwrap()))
    }

    pub fn i16(&mut self) -> Result<i16, OutOfBounds> {
        Ok(i16::from_le_bytes(self.bytes(2)?.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> Result<u32, OutOfBounds> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    pub fn i32(&mut self) -> Result<i32, OutOfBounds> {
        Ok(i32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    pub fn f32(&mut self) -> Result<f32, OutOfBounds> {
        Ok(f32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    /// Reads a NUL-terminated string at an absolute position, without
    /// moving `self.pos`. Invalid UTF-8 is replaced (`from_utf8_lossy`),
    /// matching `vpk::read_tree_cstr`'s leniency for hostile/foreign text.
    pub fn cstr_at(&self, at: usize) -> Result<String, OutOfBounds> {
        let rest = self.buf.get(at..).ok_or(OutOfBounds {
            offset: at,
            len: 1,
            size: self.buf.len(),
        })?;
        let nul = rest.iter().position(|&b| b == 0).ok_or(OutOfBounds {
            offset: at,
            len: rest.len() + 1,
            size: self.buf.len(),
        })?;
        Ok(String::from_utf8_lossy(&rest[..nul]).into_owned())
    }

    /// Source 2's "offset string": an `i32` relative to the position of the
    /// field itself (0 means empty), pointing at a NUL-terminated string
    /// elsewhere in the buffer. Leaves `self.pos` right after the `i32`
    /// field, so a struct of these can be read like any other fixed field
    /// (`StreamHelpers.cs:52-69`).
    pub fn offset_string(&mut self) -> Result<String, OutOfBounds> {
        let field_pos = self.pos;
        let raw = self.i32()?;
        if raw == 0 {
            return Ok(String::new());
        }
        let at = if raw >= 0 {
            field_pos.checked_add(raw as usize)
        } else {
            field_pos.checked_sub((-raw) as usize)
        };
        let at = at.ok_or(OutOfBounds {
            offset: field_pos,
            len: 4,
            size: self.buf.len(),
        })?;
        self.cstr_at(at)
    }
}
