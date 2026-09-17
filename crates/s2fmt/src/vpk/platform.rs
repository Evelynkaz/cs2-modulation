//! Positional reads that do not move (or need exclusive access to) a shared
//! file cursor, so many threads can read the same [`std::fs::File`]
//! concurrently without a lock around the syscall itself.
//!
//! Layout facts do not apply here; this is purely a portability shim over
//! `FileExt::seek_read` (Windows) / `FileExt::read_exact_at` (Unix), as
//! required by the spec for `Vpk::read`'s hot path.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;

/// Reads exactly `buf.len()` bytes from `file` at `offset`, without touching
/// the file's shared cursor. Returns `UnexpectedEof` if the file is shorter
/// than `offset + buf.len()`.
#[cfg(windows)]
pub(crate) fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;

    let mut total = 0usize;
    while total < buf.len() {
        let read = file.seek_read(&mut buf[total..], offset + total as u64)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unexpected end of file",
            ));
        }
        total += read;
    }
    Ok(())
}

/// Reads exactly `buf.len()` bytes from `file` at `offset`, without touching
/// the file's shared cursor.
#[cfg(unix)]
pub(crate) fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

/// If `name` ends with `suffix` (ASCII, case-insensitive), returns `name`
/// with that suffix removed. Compares at the code-unit/byte level so it
/// never lossily re-encodes the rest of the name — unlike
/// `OsStr::to_string_lossy`, this can't corrupt a non-Unicode file name
/// (e.g. one with an unpaired UTF-16 surrogate on Windows) just because we
/// only wanted to check its ASCII extension.
#[cfg(windows)]
pub(crate) fn strip_ascii_suffix(name: &OsStr, suffix: &str) -> Option<OsString> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    debug_assert!(suffix.is_ascii());
    let units: Vec<u16> = name.encode_wide().collect();
    let suffix_units: Vec<u16> = suffix.encode_utf16().collect();
    if units.len() < suffix_units.len() {
        return None;
    }
    let split = units.len() - suffix_units.len();
    let tail = &units[split..];
    let matches = tail.iter().zip(&suffix_units).all(|(&a, &b)| {
        if a < 128 && b < 128 {
            (a as u8).eq_ignore_ascii_case(&(b as u8))
        } else {
            a == b
        }
    });
    if matches {
        Some(OsString::from_wide(&units[..split]))
    } else {
        None
    }
}

/// If `name` ends with `suffix` (ASCII, case-insensitive), returns `name`
/// with that suffix removed, without lossily re-encoding the rest of the
/// name. See the Windows overload's doc comment for why this matters.
#[cfg(unix)]
pub(crate) fn strip_ascii_suffix(name: &OsStr, suffix: &str) -> Option<OsString> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    debug_assert!(suffix.is_ascii());
    let bytes = name.as_bytes();
    let suffix_bytes = suffix.as_bytes();
    if bytes.len() < suffix_bytes.len() {
        return None;
    }
    let split = bytes.len() - suffix_bytes.len();
    if bytes[split..].eq_ignore_ascii_case(suffix_bytes) {
        Some(OsString::from_vec(bytes[..split].to_vec()))
    } else {
        None
    }
}
