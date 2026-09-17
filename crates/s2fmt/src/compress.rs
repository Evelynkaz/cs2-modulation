//! LZ4 block, chained LZ4, zstd and Valve BlockCompress decoders.
//!
//! All entry points here are panic-free on arbitrary input: malformed or
//! truncated data must produce an `Err`, never a panic or an out-of-bounds
//! read/write. See `docs/FORMATS.md` section 5.

/// An error produced while decompressing a KV3 buffer.
#[derive(Debug, thiserror::Error)]
pub enum DecompressError {
    /// A raw LZ4 block failed to decompress (corrupt stream, bad back-reference, etc).
    #[error(
        "lz4 block decode failed (input {input_len} bytes, expected output {expected} bytes): {source}"
    )]
    Lz4Block {
        input_len: usize,
        expected: usize,
        #[source]
        source: lz4_flex::block::DecompressError,
    },
    /// A raw LZ4 block decoded to a size different from what was expected.
    #[error("lz4 block decoded to {actual} bytes, expected exactly {expected}")]
    Lz4SizeMismatch { actual: usize, expected: usize },
    /// The chained LZ4 blob decoder produced fewer or more total bytes than declared.
    #[error("lz4 chain produced {actual} total bytes, expected {expected}")]
    Lz4ChainTotalMismatch { actual: usize, expected: usize },
    /// [`Lz4ChainDecoder::decode_block`] was called after the expected total was already reached.
    #[error("lz4 chain decoder already has {have} of {expected} expected bytes")]
    Lz4ChainOverrun { have: usize, expected: usize },
    /// A zstd frame failed to decode.
    #[error("zstd frame decode failed: {0}")]
    Zstd(String),
    /// A zstd frame decoded to a size different from what was expected.
    #[error("zstd frame decoded to {actual} bytes, expected exactly {expected}")]
    ZstdSizeMismatch { actual: usize, expected: usize },
    /// The BlockCompress input ended before the header or a token/literal could be read.
    #[error("block-compress input truncated at offset {offset}")]
    BlockCompressTruncated { offset: usize },
    /// A BlockCompress back-reference points before the start of the output buffer.
    #[error(
        "block-compress back-reference at output position {position} underflows with offset {offset}"
    )]
    BlockCompressOffsetUnderflow { position: usize, offset: usize },
    /// A BlockCompress token would write past the declared decompressed size.
    #[error("block-compress output overrun: position {position} exceeds declared size {size}")]
    BlockCompressOverrun { position: usize, size: usize },
    /// A declared decompressed size is implausibly large (e.g. a corrupted or negative header
    /// field cast to `usize`), rejected before allocating rather than risking an allocation
    /// failure (which aborts the process instead of returning an `Err`).
    #[error("declared decompressed size {requested} exceeds the sanity limit of {limit} bytes")]
    SizeTooLarge { requested: usize, limit: usize },
}

/// Allocation sanity cap for a single decompressed buffer: real KV3 buffers are at most a few
/// MiB (see `docs/FORMATS.md`'s note on `world_physics` PHYS blocks being ~3.5 MB); this is far
/// above any legitimate size while still refusing to allocate gigabytes for corrupt input.
const MAX_DECOMPRESSED_SIZE: usize = 1 << 30;

fn check_size(requested: usize) -> Result<(), DecompressError> {
    if requested > MAX_DECOMPRESSED_SIZE {
        return Err(DecompressError::SizeTooLarge {
            requested,
            limit: MAX_DECOMPRESSED_SIZE,
        });
    }
    Ok(())
}

/// Decodes a single raw (headerless) LZ4 block into exactly `uncompressed_size` bytes.
pub fn lz4_block(input: &[u8], uncompressed_size: usize) -> Result<Vec<u8>, DecompressError> {
    check_size(uncompressed_size)?;
    // A tiny input cannot legitimately decode to a huge output: LZ4's literal-length encoding
    // needs roughly one extra input byte per additional 255 bytes of literal it represents, so
    // reject anything past that worst-case expansion ratio before allocating (rather than
    // allocating up to `MAX_DECOMPRESSED_SIZE` for a handful of input bytes).
    if uncompressed_size > input.len().saturating_mul(255).saturating_add(16) {
        return Err(DecompressError::SizeTooLarge {
            requested: uncompressed_size,
            limit: input.len().saturating_mul(255).saturating_add(16),
        });
    }
    let mut out = vec![0u8; uncompressed_size];
    let written = lz4_flex::block::decompress_into(input, &mut out).map_err(|source| {
        DecompressError::Lz4Block {
            input_len: input.len(),
            expected: uncompressed_size,
            source,
        }
    })?;
    if written != uncompressed_size {
        return Err(DecompressError::Lz4SizeMismatch {
            actual: written,
            expected: uncompressed_size,
        });
    }
    Ok(out)
}

/// Decodes the KV3 v2+ blob stream: a chain of raw LZ4 blocks, each decoded using up to the
/// last 64 KiB of previously produced output as an external dictionary. Each block decodes to
/// at most 16384 bytes (FORMATS.md 3.10), clipped to the remaining total; a block may decode to
/// *fewer* bytes than that upper bound (e.g. when it ends on a blob boundary), since raw LZ4
/// blocks have no length prefix and simply stop once their compressed token stream is consumed.
pub struct Lz4ChainDecoder {
    expected_total: usize,
    out: Vec<u8>,
}

/// The chained LZ4 dictionary window, per FORMATS.md 3.10 ("словарь — ранее распакованный
/// выход (до 64 KiB)").
const LZ4_CHAIN_DICT_WINDOW: usize = 64 * 1024;

impl Lz4ChainDecoder {
    /// Creates a decoder that expects `expected_total` decoded bytes overall.
    pub fn new(expected_total: usize) -> Self {
        Lz4ChainDecoder {
            expected_total,
            out: Vec::with_capacity(expected_total.min(1 << 20)),
        }
    }

    /// Decodes one compressed block, appending up to `max_out` bytes (clipped to the remaining
    /// expected total) to the running output. Returns the number of bytes decoded.
    pub fn decode_block(
        &mut self,
        compressed: &[u8],
        max_out: usize,
    ) -> Result<usize, DecompressError> {
        if self.out.len() >= self.expected_total {
            return Err(DecompressError::Lz4ChainOverrun {
                have: self.out.len(),
                expected: self.expected_total,
            });
        }

        let remaining = self.expected_total - self.out.len();
        let want = max_out.min(remaining);
        let mut buf = vec![0u8; want];

        let dict_start = self.out.len().saturating_sub(LZ4_CHAIN_DICT_WINDOW);
        let dict = &self.out[dict_start..];

        let written = lz4_flex::block::decompress_into_with_dict(compressed, &mut buf, dict)
            .map_err(|source| DecompressError::Lz4Block {
                input_len: compressed.len(),
                expected: want,
                source,
            })?;
        // Unlike a standalone `lz4_block` call, a chained frame is allowed to decode to *fewer*
        // than `want` bytes: `want` is only an upper bound sized from the frame-size cadence
        // (FORMATS.md 3.10's "min(16384, remaining)" rule), but the compressed block itself
        // fully determines how much plaintext it represents (raw LZ4 has no length prefix, it
        // just decodes until the input token stream is exhausted). VRF's reader accepts any
        // `decoded >= 1` here (`BinaryKV3.cs`'s `DecodeAndDrain` call) rather than requiring an
        // exact match; real compiled files rely on this (blob boundaries can end a frame early).
        if written == 0 {
            return Err(DecompressError::Lz4SizeMismatch {
                actual: written,
                expected: want,
            });
        }

        self.out.extend_from_slice(&buf[..written]);
        Ok(written)
    }

    /// Finishes the chain, checking that exactly the expected total was produced.
    pub fn finish(self) -> Result<Vec<u8>, DecompressError> {
        if self.out.len() != self.expected_total {
            return Err(DecompressError::Lz4ChainTotalMismatch {
                actual: self.out.len(),
                expected: self.expected_total,
            });
        }
        Ok(self.out)
    }
}

/// Decodes a single zstd frame into exactly `expected_size` bytes.
pub fn zstd_frame(input: &[u8], expected_size: usize) -> Result<Vec<u8>, DecompressError> {
    check_size(expected_size)?;
    let mut decoder = ruzstd::decoding::FrameDecoder::new();
    let mut out = Vec::with_capacity(expected_size);
    decoder
        .decode_all_to_vec(input, &mut out)
        .map_err(|e| DecompressError::Zstd(e.to_string()))?;
    if out.len() != expected_size {
        return Err(DecompressError::ZstdSizeMismatch {
            actual: out.len(),
            expected: expected_size,
        });
    }
    Ok(out)
}

/// Decompresses Valve's `BlockCompress` format (FORMATS.md 5), used for KV3 v0 `binary_bc` and
/// SNAP. Returns the decompressed bytes and the number of input bytes consumed.
///
/// Unlike VRF's reference implementation, back-references and literal writes here are
/// bounds-checked: a corrupt stream returns an error instead of over-reading/over-writing.
pub fn block_compress(input: &[u8]) -> Result<(Vec<u8>, usize), DecompressError> {
    if input.len() < 4 {
        return Err(DecompressError::BlockCompressTruncated { offset: 0 });
    }
    let hdr = u32::from_le_bytes([input[0], input[1], input[2], input[3]]);
    let mut pos_in = 4usize;

    // Valve sets the top bit of the header to mark an uncompressed (stored) buffer.
    if hdr & 0x8000_0000 != 0 {
        let size = (hdr & 0x7FFF_FFFF) as usize;
        let end = pos_in
            .checked_add(size)
            .ok_or(DecompressError::BlockCompressTruncated { offset: pos_in })?;
        if end > input.len() {
            return Err(DecompressError::BlockCompressTruncated { offset: pos_in });
        }
        return Ok((input[pos_in..end].to_vec(), end));
    }

    let size = hdr as usize;
    check_size(size)?;
    // As with `lz4_block`, reject a declared size that a tiny input couldn't plausibly produce:
    // each mask token group (16 tokens) is at most 2 (mask) + 16*2 (back-reference tokens) = 34
    // input bytes producing at most 16*18 = 288 output bytes, a ~8.5x ratio; 9x leaves headroom.
    let max_plausible = input
        .len()
        .saturating_sub(4)
        .saturating_mul(9)
        .saturating_add(16);
    if size > max_plausible {
        return Err(DecompressError::SizeTooLarge {
            requested: size,
            limit: max_plausible,
        });
    }
    let mut out = vec![0u8; size];
    let mut position = 0usize;
    let mut mask: u16 = 0;
    let mut bits_left: u8 = 0;

    while position < size {
        if bits_left == 0 {
            if pos_in + 2 > input.len() {
                return Err(DecompressError::BlockCompressTruncated { offset: pos_in });
            }
            mask = u16::from_le_bytes([input[pos_in], input[pos_in + 1]]);
            pos_in += 2;
            bits_left = 16;
        }

        if mask & 1 != 0 {
            if pos_in + 2 > input.len() {
                return Err(DecompressError::BlockCompressTruncated { offset: pos_in });
            }
            let tok = u16::from_le_bytes([input[pos_in], input[pos_in + 1]]);
            pos_in += 2;
            let offset = ((tok >> 4) + 1) as usize;
            let len = ((tok & 0xF) + 3) as usize;

            if offset > position {
                return Err(DecompressError::BlockCompressOffsetUnderflow { position, offset });
            }
            let mut src = position - offset;
            for _ in 0..len {
                if position >= size {
                    return Err(DecompressError::BlockCompressOverrun { position, size });
                }
                // Overlapping copies are intentional (run-length style back-references); src
                // trails position by `offset` even as both advance, matching VRF's byte-by-byte
                // copy loop (Compression/BlockCompress.cs).
                out[position] = out[src];
                position += 1;
                src += 1;
            }
        } else {
            if pos_in >= input.len() {
                return Err(DecompressError::BlockCompressTruncated { offset: pos_in });
            }
            if position >= size {
                return Err(DecompressError::BlockCompressOverrun { position, size });
            }
            out[position] = input[pos_in];
            pos_in += 1;
            position += 1;
        }

        mask >>= 1;
        bits_left -= 1;
    }

    Ok((out, pos_in))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lz4_block_roundtrip() {
        let data = b"hello hello hello hello world world world".repeat(4);
        let compressed = lz4_flex::block::compress(&data);
        let out = lz4_block(&compressed, data.len()).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn lz4_block_size_mismatch_errors() {
        let data = b"abcabcabcabc".to_vec();
        let compressed = lz4_flex::block::compress(&data);
        assert!(lz4_block(&compressed, data.len() + 5).is_err());
    }

    #[test]
    fn lz4_block_rejects_implausible_size_for_tiny_input() {
        // A ~1 GiB request from a couple of input bytes must be rejected before allocating.
        let input = [0u8, 0u8];
        assert!(matches!(
            lz4_block(&input, 0x7FFF_FFFF),
            Err(DecompressError::SizeTooLarge { .. })
        ));
    }

    #[test]
    fn block_compress_rejects_implausible_size_for_tiny_input() {
        // header claims a huge decompressed size backed by almost no compressed data.
        let mut input = 0x0800_0000u32.to_le_bytes().to_vec(); // size = 128 MiB
        input.extend_from_slice(&[0u8; 4]);
        assert!(matches!(
            block_compress(&input),
            Err(DecompressError::SizeTooLarge { .. })
        ));
    }

    #[test]
    fn lz4_block_garbage_does_not_panic() {
        for len in 0..40 {
            let input = vec![0x42u8; len];
            let _ = lz4_block(&input, 64);
        }
    }

    #[test]
    fn lz4_chain_roundtrip_single_block() {
        let data = b"the quick brown fox jumps over the lazy dog".repeat(3);
        let compressed = lz4_flex::block::compress(&data);
        let mut dec = Lz4ChainDecoder::new(data.len());
        let n = dec.decode_block(&compressed, data.len()).unwrap();
        assert_eq!(n, data.len());
        assert_eq!(dec.finish().unwrap(), data);
    }

    #[test]
    fn lz4_chain_roundtrip_multi_block_with_dict() {
        // Build a stream compressed as consecutive dictionary-chained blocks, then decode it.
        let mut plain = Vec::new();
        for i in 0..5u8 {
            plain.extend(std::iter::repeat_n(i, 5000));
        }
        let frame_size = 4096usize;
        let mut compressed_blocks = Vec::new();
        let mut offset = 0;
        let mut prev: Vec<u8> = Vec::new();
        while offset < plain.len() {
            let end = (offset + frame_size).min(plain.len());
            let chunk = &plain[offset..end];
            let dict_start = prev.len().saturating_sub(64 * 1024);
            let dict = &prev[dict_start..];
            let compressed = lz4_flex::block::compress_with_dict(chunk, dict);
            compressed_blocks.push((compressed, chunk.len()));
            prev.extend_from_slice(chunk);
            offset = end;
        }

        let mut dec = Lz4ChainDecoder::new(plain.len());
        for (compressed, len) in &compressed_blocks {
            dec.decode_block(compressed, *len).unwrap();
        }
        assert_eq!(dec.finish().unwrap(), plain);
    }

    #[test]
    fn lz4_chain_garbage_does_not_panic() {
        for len in 0..40 {
            let input = vec![0x11u8; len];
            let mut dec = Lz4ChainDecoder::new(100);
            let _ = dec.decode_block(&input, 16384);
        }
    }

    #[test]
    fn lz4_chain_overrun_errors() {
        let data = vec![1u8, 2, 3, 4];
        let compressed = lz4_flex::block::compress(&data);
        let mut dec = Lz4ChainDecoder::new(4);
        dec.decode_block(&compressed, 4).unwrap();
        assert!(dec.decode_block(&compressed, 4).is_err());
    }

    fn zstd_compress(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        ruzstd::encoding::compress(
            std::io::Cursor::new(data),
            &mut out,
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        out
    }

    #[test]
    fn zstd_frame_roundtrip() {
        let data = b"zstd zstd zstd payload payload payload".repeat(8);
        let compressed = zstd_compress(&data);
        let out = zstd_frame(&compressed, data.len()).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn zstd_frame_size_mismatch_errors() {
        let data = b"zstd data here".to_vec();
        let compressed = zstd_compress(&data);
        assert!(zstd_frame(&compressed, data.len() + 1).is_err());
    }

    #[test]
    fn zstd_frame_garbage_does_not_panic() {
        for len in 0..40 {
            let input = vec![0x77u8; len];
            let _ = zstd_frame(&input, 64);
        }
    }

    #[test]
    fn block_compress_stored() {
        let payload = b"stored as-is";
        let mut input = ((payload.len() as u32) | 0x8000_0000)
            .to_le_bytes()
            .to_vec();
        input.extend_from_slice(payload);
        let (out, consumed) = block_compress(&input).unwrap();
        assert_eq!(out, payload);
        assert_eq!(consumed, input.len());
    }

    #[test]
    fn block_compress_literals_only() {
        let payload = b"abcd";
        let mut input = (payload.len() as u32).to_le_bytes().to_vec();
        input.extend_from_slice(&0u16.to_le_bytes()); // all-literal mask
        input.extend_from_slice(payload);
        let (out, _) = block_compress(&input).unwrap();
        assert_eq!(out, payload);
    }

    #[test]
    fn block_compress_backreference_rle() {
        // size=6, mask selects: literal 'a', then a back-reference token (offset=1,len=5)
        // producing "aaaaaa".
        let mut input = 6u32.to_le_bytes().to_vec();
        input.extend_from_slice(&0b10u16.to_le_bytes());
        input.push(b'a');
        // token: offset=1 -> stored (offset-1)=0; len=5 -> stored (len-3)=2
        let tok: u16 = 2u16;
        input.extend_from_slice(&tok.to_le_bytes());
        let (out, _) = block_compress(&input).unwrap();
        assert_eq!(out, b"aaaaaa");
    }

    #[test]
    fn block_compress_bad_offset_errors() {
        // Back-reference before any literal has been written: offset underflows.
        let mut input = 4u32.to_le_bytes().to_vec();
        input.extend_from_slice(&0b1u16.to_le_bytes());
        let tok: u16 = 0u16;
        input.extend_from_slice(&tok.to_le_bytes());
        assert!(block_compress(&input).is_err());
    }

    #[test]
    fn block_compress_truncated_does_not_panic() {
        for len in 0..20 {
            let input = vec![0x55u8; len];
            let _ = block_compress(&input);
        }
    }

    #[test]
    fn block_compress_fuzzish_random_does_not_panic() {
        // Deterministic pseudo-random bytes, no external dependency needed.
        let mut state: u32 = 0x1234_5678;
        for _ in 0..200 {
            let mut input = Vec::new();
            for _ in 0..64 {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12345);
                input.push((state >> 16) as u8);
            }
            let _ = block_compress(&input);
        }
    }
}
