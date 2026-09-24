//! Encodes a linear HDR image as a Radiance `.hdr` (RGBE) file
//! (`s6f3a2b_hdr.md` change item 4).
//!
//! Chosen over RGBM-in-PNG because three.js ships `RGBELoader` for exactly
//! this format out of the box (`examples/jsm/loaders/RGBELoader.js`), with
//! no custom shader/unpack step needed on the browser side, unlike RGBM
//! (which three.js has no built-in loader for at all -- it would need a
//! bespoke PNG-plus-unpack-shader pipeline). The trade-off is precision:
//! RGBE's shared 8-bit exponent gives each pixel roughly the relative
//! precision of an 8-bit mantissa (~1/256, comparable to half-float's ~10
//! bits, plenty for a sky/lightmap preview), at 4 bytes/pixel before the
//! RLE below shrinks it further.
//!
//! Scanlines wider than 8 and narrower than 32768 use the "new-style" RLE
//! encoding real `.hdr` writers use (each of R/G/B/E run-length encoded
//! separately per row); narrower/wider rows fall back to flat per-pixel
//! bytes, which is also what a compliant reader expects outside that width
//! range (this is the same width gate `RGBELoader` itself uses to decide
//! whether a row can even be RLE-encoded).

const RLE_MIN_WIDTH: u32 = 8;
const RLE_MAX_WIDTH: u32 = 0x7fff;
const MIN_RUN: usize = 4;
const MAX_RUN: usize = 127;
const MAX_DUMP: usize = 128;

/// `(mantissa, exponent)` such that `mantissa * 2^exponent == v` and
/// `mantissa` is in `[0.5, 1)`, for `v > 0` -- Radiance's own encoding
/// convention (`float2rgbe`'s `frexp`), built by directly rewriting the
/// IEEE 754 exponent field rather than calling libm, so it's exact for any
/// normal `f32` (every `v` this module calls it with has already been
/// clamped away from the subnormal range by the `< 1e-32` guard below).
fn frexp(v: f32) -> (f32, i32) {
    let bits = v.to_bits();
    let exp_field = (bits >> 23) & 0xff;
    let exp = exp_field as i32 - 126; // v = mantissa * 2^exp, mantissa in [0.5, 1)
    let mantissa_bits = (bits & 0x807f_ffff) | (126u32 << 23);
    (f32::from_bits(mantissa_bits), exp)
}

/// One pixel's linear RGB to RGBE bytes (Radiance's `float2rgbe`).
fn pixel_to_rgbe(r: f32, g: f32, b: f32) -> [u8; 4] {
    let max = r.max(g).max(b).max(0.0);
    if max < 1e-32 {
        return [0, 0, 0, 0];
    }
    let (mantissa, exp) = frexp(max);
    let scale = mantissa * 256.0 / max;
    [
        (r.max(0.0) * scale) as u8,
        (g.max(0.0) * scale) as u8,
        (b.max(0.0) * scale) as u8,
        (exp + 128) as u8,
    ]
}

/// Run-length encodes one channel's `width` bytes (one row), the standard
/// "new-style" Radiance scheme: a byte `> 128` introduces a run of
/// `byte - 128` repeats of the next byte; a byte `<= 128` introduces that
/// many literal bytes.
fn rle_encode_channel(out: &mut Vec<u8>, channel: &[u8]) {
    let n = channel.len();
    let mut i = 0usize;
    while i < n {
        let mut run = 1usize;
        while i + run < n && run < MAX_RUN && channel[i + run] == channel[i] {
            run += 1;
        }
        if run >= MIN_RUN {
            out.push((128 + run) as u8);
            out.push(channel[i]);
            i += run;
            continue;
        }

        let dump_start = i;
        let mut dump_len = 0usize;
        while i < n && dump_len < MAX_DUMP {
            let mut r = 1usize;
            while i + r < n && r < MAX_RUN && channel[i + r] == channel[i] {
                r += 1;
            }
            if r >= MIN_RUN {
                break;
            }
            i += 1;
            dump_len += 1;
        }
        out.push(dump_len as u8);
        out.extend_from_slice(&channel[dump_start..dump_start + dump_len]);
    }
}

fn write_scanline(out: &mut Vec<u8>, width: u32, rgbe_row: &[[u8; 4]]) {
    if !(RLE_MIN_WIDTH..=RLE_MAX_WIDTH).contains(&width) {
        for px in rgbe_row {
            out.extend_from_slice(px);
        }
        return;
    }

    out.extend_from_slice(&[2, 2, (width >> 8) as u8, width as u8]);
    for channel in 0..4 {
        let bytes: Vec<u8> = rgbe_row.iter().map(|px| px[channel]).collect();
        rle_encode_channel(out, &bytes);
    }
}

/// Encodes `rgba` (`width * height * 4` linear floats, alpha ignored --
/// Radiance has no alpha channel) as a Radiance `.hdr` file.
pub fn encode(width: u32, height: u32, rgba: &[f32]) -> Vec<u8> {
    let pixel_count = (width as usize) * (height as usize);
    debug_assert_eq!(rgba.len(), pixel_count * 4);

    let mut out = Vec::new();
    out.extend_from_slice(b"#?RADIANCE\n");
    out.extend_from_slice(b"FORMAT=32-bit_rle_rgbe\n\n");
    out.extend_from_slice(format!("-Y {height} +X {width}\n").as_bytes());

    let mut row = Vec::with_capacity(width as usize);
    for y in 0..height {
        row.clear();
        for x in 0..width {
            let i = ((y * width + x) as usize) * 4;
            row.push(pixel_to_rgbe(rgba[i], rgba[i + 1], rgba[i + 2]));
        }
        write_scanline(&mut out, width, &row);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads back exactly what `write_scanline`/`rle_encode_channel` wrote,
    /// mirroring the disambiguation a compliant reader (three.js's
    /// `RGBELoader` included) uses: a scanline starting with `[2, 2, hi,
    /// lo]` whose `hi/lo` matches `width` is new-style RLE, anything else is
    /// flat pixel bytes.
    fn decode_scanline(data: &[u8], width: u32) -> (Vec<[u8; 4]>, usize) {
        if (RLE_MIN_WIDTH..=RLE_MAX_WIDTH).contains(&width)
            && data.len() >= 4
            && data[0] == 2
            && data[1] == 2
            && ((data[2] as u32) << 8 | data[3] as u32) == width
        {
            let mut pos = 4usize;
            let mut channels = [vec![0u8; width as usize], vec![], vec![], vec![]];
            channels[1] = vec![0u8; width as usize];
            channels[2] = vec![0u8; width as usize];
            channels[3] = vec![0u8; width as usize];
            for channel in channels.iter_mut() {
                let mut x = 0usize;
                while x < width as usize {
                    let count = data[pos] as usize;
                    pos += 1;
                    if count > 128 {
                        let run = count - 128;
                        let value = data[pos];
                        pos += 1;
                        for _ in 0..run {
                            channel[x] = value;
                            x += 1;
                        }
                    } else {
                        for _ in 0..count {
                            channel[x] = data[pos];
                            pos += 1;
                            x += 1;
                        }
                    }
                }
            }
            let pixels = (0..width as usize)
                .map(|x| {
                    [
                        channels[0][x],
                        channels[1][x],
                        channels[2][x],
                        channels[3][x],
                    ]
                })
                .collect();
            (pixels, pos)
        } else {
            let pixels: Vec<[u8; 4]> = data[..(width as usize) * 4].as_chunks::<4>().0.to_vec();
            let consumed = pixels.len() * 4;
            (pixels, consumed)
        }
    }

    #[test]
    fn header_has_the_expected_radiance_shape() {
        let bytes = encode(8, 1, &[1.0f32, 0.0, 0.0, 1.0].repeat(8));
        let text = String::from_utf8_lossy(&bytes[..64.min(bytes.len())]);
        assert!(text.starts_with("#?RADIANCE\n"));
        assert!(text.contains("FORMAT=32-bit_rle_rgbe\n"));
        assert!(text.contains("-Y 1 +X 8\n"));
    }

    #[test]
    fn pixel_round_trips_through_rle_within_rgbe_precision() {
        let width = 16u32;
        let mut rgba = Vec::new();
        for x in 0..width {
            let t = x as f32 / width as f32;
            rgba.extend_from_slice(&[t * 4.0, t * 2.0, 1.0 - t, 1.0]);
        }
        let bytes = encode(width, 1, &rgba);

        let header_end = bytes.windows(2).position(|w| w == b"\n\n").unwrap() + 2;
        let line_end = bytes[header_end..]
            .iter()
            .position(|&b| b == b'\n')
            .unwrap()
            + header_end
            + 1;
        let (pixels, consumed) = decode_scanline(&bytes[line_end..], width);
        assert_eq!(consumed, bytes.len() - line_end);
        assert_eq!(pixels.len(), width as usize);

        for (x, rgbe) in pixels.iter().enumerate() {
            let expected = pixel_to_rgbe(rgba[x * 4], rgba[x * 4 + 1], rgba[x * 4 + 2]);
            assert_eq!(*rgbe, expected, "pixel {x}");
        }
    }

    #[test]
    fn narrow_row_uses_flat_encoding() {
        let width = 4u32; // below RLE_MIN_WIDTH
        let rgba = [0.5f32, 0.5, 0.5, 1.0].repeat(width as usize);
        let bytes = encode(width, 1, &rgba);
        // Flat: no [2,2,hi,lo] marker, exactly 4 bytes/pixel after the header.
        let header_end = bytes.windows(2).position(|w| w == b"\n\n").unwrap() + 2;
        let line_end = bytes[header_end..]
            .iter()
            .position(|&b| b == b'\n')
            .unwrap()
            + header_end
            + 1;
        assert_eq!(bytes.len() - line_end, (width as usize) * 4);
    }

    #[test]
    fn zero_pixel_encodes_to_zero_rgbe() {
        assert_eq!(pixel_to_rgbe(0.0, 0.0, 0.0), [0, 0, 0, 0]);
    }

    /// Pins the exponent bias (`+128` in `pixel_to_rgbe`, `frexp`'s `-126`)
    /// and the mantissa scale (`256.0`) to known values: changing `-126` to
    /// `-127` or `+128` to `+127` would shift every decoded pixel by 2x
    /// without failing any test that only checks round-trip precision
    /// (`s6f3a2b_hdr.md`'s regression note).
    #[test]
    fn pixel_to_rgbe_pins_known_values() {
        assert_eq!(pixel_to_rgbe(1.0, 0.5, 0.25), [128, 64, 32, 129]);
        assert_eq!(pixel_to_rgbe(3.0, 0.0, 0.0), [192, 0, 0, 130]);
        assert_eq!(pixel_to_rgbe(0.75, 0.0, 0.0), [192, 0, 0, 128]);
    }

    /// The Radiance decode formula (`mantissa * 2^(e - 136)`, the standard
    /// inverse of `float2rgbe`'s `scale = mantissa * 256 / max`) applied to
    /// each channel -- pins the encode's exponent bias and mantissa scale
    /// together, catching a change to either even if it left
    /// `pixel_to_rgbe`'s own byte output looking plausible in isolation.
    fn radiance_decode(rgbe: [u8; 4]) -> [f32; 3] {
        let scale = 2f32.powi(i32::from(rgbe[3]) - 136);
        [
            f32::from(rgbe[0]) * scale,
            f32::from(rgbe[1]) * scale,
            f32::from(rgbe[2]) * scale,
        ]
    }

    #[test]
    fn radiance_decode_recovers_known_inputs() {
        assert_eq!(
            radiance_decode(pixel_to_rgbe(1.0, 0.5, 0.25)),
            [1.0, 0.5, 0.25]
        );
        assert_eq!(
            radiance_decode(pixel_to_rgbe(3.0, 0.0, 0.0)),
            [3.0, 0.0, 0.0]
        );
        assert_eq!(
            radiance_decode(pixel_to_rgbe(0.75, 0.0, 0.0)),
            [0.75, 0.0, 0.0]
        );
    }

    #[test]
    fn negative_channels_clamp_to_zero() {
        let rgbe = pixel_to_rgbe(-1.0, 1.0, -1.0);
        assert_eq!(rgbe[0], 0);
        assert_eq!(rgbe[2], 0);
        assert!(rgbe[1] > 0);
    }

    #[test]
    fn does_not_panic_on_a_degenerate_1x1_image() {
        let bytes = encode(1, 1, &[0.0, 0.0, 0.0, 1.0]);
        assert!(!bytes.is_empty());
    }
}
