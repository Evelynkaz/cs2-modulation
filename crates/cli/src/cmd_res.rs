//! `cs2mod res <FILE> [--vpk <VPK>] [--block <4CC>] [--kv3]` dev command.

use std::io::{self, Write};
use std::path::Path;

use anyhow::Context;
use s2fmt::kv3;
use s2fmt::resource::{Block, FourCC, Resource};
use s2fmt::vpk::Vpk;

use crate::game_path::resolve_vpk_path;

/// Reads `file`'s bytes: either a path on disk, or an entry path inside
/// `vpk` (resolved via `--game`/`CS2_GAME_DIR` if `vpk` names a bare VPK).
fn read_bytes(file: &str, vpk: Option<&str>, game: Option<&Path>) -> anyhow::Result<Vec<u8>> {
    match vpk {
        Some(vpk_name) => {
            let path = resolve_vpk_path(vpk_name, game)?;
            let archive =
                Vpk::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
            archive
                .read_path(file)
                .with_context(|| format!("failed to read {file:?} from {}", path.display()))
        }
        None => std::fs::read(file).with_context(|| format!("failed to read {file}")),
    }
}

/// Parses a `--block XXXX` argument into a [`FourCC`]: exactly 4 ASCII
/// bytes.
fn parse_fourcc(s: &str) -> anyhow::Result<FourCC> {
    let bytes = s.as_bytes();
    anyhow::ensure!(
        bytes.len() == 4 && bytes.is_ascii(),
        "--block must be exactly 4 ASCII characters, got {s:?}"
    );
    Ok(FourCC([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn print_block_table(out: &mut impl Write, res: &Resource) -> io::Result<()> {
    writeln!(
        out,
        "header: file_size={} header_version={} version={}",
        res.file_size(),
        res.header_version(),
        res.version()
    )?;
    if let Some((declared, actual)) = res.file_size_mismatch() {
        writeln!(
            out,
            "  (warning: header file_size {declared} != actual byte length {actual})"
        )?;
    }
    writeln!(out, "blocks:")?;
    for block in res.blocks() {
        let filtered = block
            .filtered_index
            .map(|i| i.to_string())
            .unwrap_or_else(|| "-".to_string());
        let kv3_note = if block.size > 0 {
            match kv3::binary_header_info(res.block_bytes(block)) {
                Some(kv3::BinaryHeaderInfo {
                    version,
                    compression: Some(c),
                }) => format!("  KV3 v{version} c={c}"),
                Some(kv3::BinaryHeaderInfo {
                    version,
                    compression: None,
                }) => format!("  KV3 v{version}"),
                None => String::new(),
            }
        } else {
            String::new()
        };
        writeln!(
            out,
            "  raw={:<4} filtered={:<4} {}  offset={:<10} size={:<10}{}",
            block.raw_index, filtered, block.fourcc, block.offset, block.size, kv3_note
        )?;
    }
    Ok(())
}

/// True if every whitespace-separated token is exactly 2 ASCII hex digits
/// (i.e. `hex_body` is really a `to_text` blob literal's payload, not, say,
/// a quoted string value that happens to contain a literal `#[...]`).
fn is_hex_byte_pairs(tokens: &[&str]) -> bool {
    tokens
        .iter()
        .all(|t| t.len() == 2 && t.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Truncates a genuine `#[...]` blob literal in `to_text`'s output past 64
/// bytes, appending `/* N bytes total */` (spec: "Large blobs in text
/// output"). Only touches a `#[...]` span whose contents are exactly
/// whitespace-separated hex byte pairs; anything else (e.g. a string value
/// that happens to contain the same characters) is left untouched.
fn truncate_blobs(text: &str, full: bool) -> String {
    if full {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("#[") {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find(']') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let hex_body = &after_open[..end];
        let tokens: Vec<&str> = hex_body.split_whitespace().collect();
        if is_hex_byte_pairs(&tokens) && tokens.len() > 64 {
            out.push_str("#[");
            out.push_str(&tokens[..64].join(" "));
            out.push_str(&format!(" ...] /* {} bytes total */", tokens.len()));
        } else {
            // Either a real (short) hex blob, or not a blob literal at all:
            // leave it exactly as it was.
            out.push_str("#[");
            out.push_str(hex_body);
            out.push(']');
        }
        rest = &after_open[end + 1..];
    }
    out.push_str(rest);
    out
}

fn print_kv3_block(
    out: &mut impl Write,
    res: &Resource,
    block: &Block,
    full_blobs: bool,
) -> anyhow::Result<()> {
    let doc = res
        .kv3(block)
        .with_context(|| format!("block {} is not valid KV3", block.fourcc))?;
    let text = kv3::to_text(&doc);
    writeln!(out, "{}", truncate_blobs(&text, full_blobs))?;
    Ok(())
}

pub struct ResArgs<'a> {
    pub file: &'a str,
    pub vpk: Option<&'a str>,
    pub game: Option<&'a Path>,
    pub block: Option<&'a str>,
    pub kv3: bool,
    pub full_blobs: bool,
}

pub fn run(args: &ResArgs) -> anyhow::Result<()> {
    let bytes = read_bytes(args.file, args.vpk, args.game)?;
    let res = Resource::parse(bytes)
        .with_context(|| format!("failed to parse {} as a resource container", args.file))?;

    let stdout = io::stdout();
    let mut out = stdout.lock();

    match (args.block, args.kv3) {
        (Some(block_str), true) => {
            let fourcc = parse_fourcc(block_str)?;
            let block = res
                .block(fourcc)
                .with_context(|| format!("no non-empty {fourcc} block"))?;
            print_kv3_block(&mut out, &res, block, args.full_blobs)?;
        }
        (Some(block_str), false) => {
            let fourcc = parse_fourcc(block_str)?;
            let block = res
                .block(fourcc)
                .with_context(|| format!("no non-empty {fourcc} block"))?;
            writeln!(
                out,
                "block {} raw={} filtered={:?} offset={} size={}",
                block.fourcc, block.raw_index, block.filtered_index, block.offset, block.size
            )?;
        }
        (None, true) => {
            print_block_table(&mut out, &res)?;
            for block in res.blocks() {
                if block.size == 0 || !kv3::is_binary_kv3(res.block_bytes(block)) {
                    continue;
                }
                writeln!(
                    out,
                    "--- block {} (raw index {}) ---",
                    block.fourcc, block.raw_index
                )?;
                print_kv3_block(&mut out, &res, block, args.full_blobs)?;
            }
        }
        (None, false) => print_block_table(&mut out, &res)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_long_hex_blob() {
        let hex: Vec<String> = (0..100u32).map(|i| format!("{:02X}", i % 256)).collect();
        let text = format!("v = #[{}]", hex.join(" "));
        let truncated = truncate_blobs(&text, false);
        assert!(truncated.contains("/* 100 bytes total */"));
        assert!(truncated.contains("...]"));
        assert_eq!(truncated.matches("...] /*").count(), 1);
    }

    #[test]
    fn leaves_short_hex_blob_untouched() {
        let text = "v = #[01 02 03]";
        assert_eq!(truncate_blobs(text, false), text);
    }

    #[test]
    fn full_blobs_flag_disables_truncation() {
        let hex: Vec<String> = (0..100u32).map(|i| format!("{:02X}", i % 256)).collect();
        let text = format!("v = #[{}]", hex.join(" "));
        assert_eq!(truncate_blobs(&text, true), text);
    }

    #[test]
    fn string_value_with_bracket_syntax_is_left_untouched() {
        // Not a blob literal at all: a string value containing "#[" and
        // many non-hex, space-separated words followed by "]". Must not be
        // mistaken for a blob and truncated.
        let words: Vec<&str> = std::iter::repeat_n("word", 100).collect();
        let text = format!("v = \"prefix #[{}] suffix\"", words.join(" "));
        assert_eq!(truncate_blobs(&text, false), text);
    }

    #[test]
    fn mixed_hex_and_non_hex_tokens_are_left_untouched() {
        let mut tokens: Vec<String> = (0..70u32).map(|i| format!("{:02X}", i % 256)).collect();
        tokens[10] = "zz".to_string(); // not a valid hex pair
        let text = format!("v = #[{}]", tokens.join(" "));
        assert_eq!(truncate_blobs(&text, false), text);
    }
}
