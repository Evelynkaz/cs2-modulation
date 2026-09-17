//! KV3 text reader/writer (FORMATS.md section 4).

use crate::kv3::guid::{ENCODING_TEXT, FORMAT_GENERIC};
use crate::kv3::{Document, Flag, Guid, Kv3Error, Kv3Version, Object, Value};

pub fn parse_text(src: &str) -> Result<Document, Kv3Error> {
    let mut p = Parser::new(src);
    let (encoding, format) = p.parse_header()?;
    p.skip_ws_and_comments()?;
    let root = p.parse_value()?;
    p.skip_ws_and_comments()?;
    if p.peek().is_some() {
        return Err(p.err("trailing data after root value"));
    }
    Ok(Document {
        format,
        encoding: Some(encoding),
        version: Kv3Version::Text,
        root,
    })
}

/// Serialises `doc` as KV3 text. The header's encoding is always `text:version{<ENCODING_TEXT>}`
/// regardless of `doc.encoding` (which describes how a *binary* document was originally
/// encoded and has no text equivalent); consequently `doc.encoding` is not preserved by a
/// `parse_text(&to_text(doc))` round trip -- the result always has `encoding: Some(ENCODING_TEXT)`.
/// The format name is written as `generic` only when `doc.format == FORMAT_GENERIC`; for any
/// other format GUID a neutral `unknown` name is used instead, since ValveKeyValue's
/// `KV3TokenReader` rejects a `generic` name paired with a non-generic GUID
/// (`KV3TokenReader.cs` ~220-225) but does not otherwise check the name against the GUID.
pub fn to_text(doc: &Document) -> String {
    let format_name = if doc.format == FORMAT_GENERIC {
        "generic"
    } else {
        "unknown"
    };
    let mut out = String::new();
    out.push_str(&format!(
        "<!-- kv3 encoding:text:version{{{ENCODING_TEXT}}} format:{format_name}:version{{{}}} -->\n",
        doc.format
    ));
    write_value(&mut out, &doc.root, 0, true);
    out.push('\n');
    out
}

fn write_value(out: &mut String, value: &Value, indent: usize, top_level: bool) {
    let flag = value.flag();
    if flag != Flag::None {
        out.push_str(flag_name(flag));
        out.push(':');
    }
    match value.unflagged() {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::UInt(u) => out.push_str(&u.to_string()),
        Value::Double(d) => out.push_str(&format_double(*d)),
        Value::String(s) => write_string(out, s),
        Value::Blob(b) => {
            out.push_str("#[");
            for (i, byte) in b.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                out.push_str(&format!("{byte:02X}"));
            }
            out.push(']');
        }
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
            } else {
                out.push_str("[\n");
                for item in items {
                    push_indent(out, indent + 1);
                    write_value(out, item, indent + 1, false);
                    out.push_str(",\n");
                }
                push_indent(out, indent);
                out.push(']');
            }
        }
        Value::Object(obj) => {
            if obj.is_empty() && !top_level {
                out.push_str("{}");
            } else {
                out.push_str("{\n");
                for (key, val) in obj.iter() {
                    push_indent(out, indent + 1);
                    write_key(out, key);
                    out.push_str(" = ");
                    write_value(out, val, indent + 1, false);
                    out.push('\n');
                }
                push_indent(out, indent);
                out.push('}');
            }
        }
        Value::Flagged(..) => unreachable!("unflagged() strips Flagged"),
    }
}

fn push_indent(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push('\t');
    }
}

fn flag_name(flag: Flag) -> &'static str {
    match flag {
        Flag::None => "",
        Flag::Resource => "resource",
        Flag::ResourceName => "resource_name",
        Flag::Panorama => "panorama",
        Flag::SoundEvent => "soundevent",
        Flag::SubClass => "subclass",
        Flag::EntityName => "entity_name",
    }
}

fn is_bare_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

fn write_key(out: &mut String, key: &str) {
    if is_bare_identifier(key) {
        out.push_str(key);
    } else {
        write_string(out, key);
    }
}

fn write_string(out: &mut String, s: &str) {
    // The multiline `"""..."""` form is only safe when the content itself doesn't contain a
    // `"""` run (which would be read back as the closing delimiter, truncating the string) and
    // doesn't end in `\r` (the reader strips a trailing `\r\n`/`\n` that it assumes we added as
    // padding; if the *content* legitimately ends in `\r` right before our own trailing `\n`,
    // that `\r` would be silently swallowed on re-parse, losing data).
    let use_multiline = s.contains('\n') && !s.contains("\"\"\"") && !s.ends_with('\r');
    if use_multiline {
        out.push_str("\"\"\"\n");
        out.push_str(s);
        out.push_str("\n\"\"\"");
    } else {
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\t' => out.push_str("\\t"),
                '\n' => out.push_str("\\n"),
                _ => out.push(c),
            }
        }
        out.push('"');
    }
}

/// Formats a double so that it always round-trips as a `Double` (never re-parsed as an
/// integer): a `.0` suffix is appended when Rust's shortest round-trip formatting would
/// otherwise produce a bare integer.
fn format_double(d: f64) -> String {
    if d.is_nan() {
        return "nan".to_string();
    }
    if d.is_infinite() {
        return if d > 0.0 {
            "inf".to_string()
        } else {
            "-inf".to_string()
        };
    }
    let s = format!("{d:?}"); // Rust's Debug for f64 is shortest round-trip and always has a `.`.
    s
}

struct Parser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    depth: u32,
}

/// Maximum array/object nesting depth. See `RECURSION_LIMIT` in `binary.rs` for why this isn't a
/// larger round number: debug builds use several KiB of stack per nesting level, so the limit is
/// set (with a safety margin) below the empirically observed ceiling rather than at an arbitrary
/// "big enough" value. See `tests::depth_at_limit_does_not_overflow_a_4mib_stack`.
///
/// Callers must parse on a thread with at least 4 MiB of stack (the CLI uses 16 MiB; the
/// server configures its worker threads likewise).
const TEXT_RECURSION_LIMIT: u32 = 128;

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Parser {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            depth: 0,
        }
    }

    fn enter_depth(&mut self) -> Result<(), Kv3Error> {
        self.depth += 1;
        if self.depth > TEXT_RECURSION_LIMIT {
            return Err(self.err(format!(
                "nesting exceeds the recursion limit of {TEXT_RECURSION_LIMIT}"
            )));
        }
        Ok(())
    }

    fn leave_depth(&mut self) {
        self.depth -= 1;
    }

    fn err(&self, message: impl Into<String>) -> Kv3Error {
        Kv3Error::TextParse {
            pos: self.pos,
            message: message.into(),
        }
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.src[self.pos..].chars().nth(offset)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn eof(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn expect_char(&mut self, c: char) -> Result<(), Kv3Error> {
        if self.peek() == Some(c) {
            self.bump();
            Ok(())
        } else {
            Err(self.err(format!("expected '{c}'")))
        }
    }

    fn skip_ws_and_comments(&mut self) -> Result<(), Kv3Error> {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.bump();
                }
                Some('/') if self.peek_at(1) == Some('/') => {
                    while !self.eof() && self.peek() != Some('\n') {
                        self.bump();
                    }
                }
                Some('/') if self.peek_at(1) == Some('*') => {
                    self.bump();
                    self.bump();
                    loop {
                        if self.eof() {
                            return Err(self.err("unterminated block comment"));
                        }
                        if self.peek() == Some('*') && self.peek_at(1) == Some('/') {
                            self.bump();
                            self.bump();
                            break;
                        }
                        self.bump();
                    }
                }
                _ => break,
            }
        }
        Ok(())
    }

    fn parse_header(&mut self) -> Result<(Guid, Guid), Kv3Error> {
        self.skip_ws_and_comments()?;
        self.expect_literal("<!--")?;
        self.skip_ws_and_comments()?;
        self.expect_keyword("kv3")?;
        self.skip_ws_and_comments()?;
        self.expect_keyword("encoding")?;
        self.expect_char(':')?;
        let _encoding_name = self.read_token()?;
        self.expect_char(':')?;
        self.expect_keyword("version")?;
        self.expect_char('{')?;
        let encoding = self.read_guid()?;
        self.expect_char('}')?;
        self.skip_ws_and_comments()?;
        self.expect_keyword("format")?;
        self.expect_char(':')?;
        let _format_name = self.read_token()?;
        self.expect_char(':')?;
        self.expect_keyword("version")?;
        self.expect_char('{')?;
        let format = self.read_guid()?;
        self.expect_char('}')?;
        self.skip_ws_and_comments()?;
        self.expect_literal("-->")?;
        Ok((encoding, format))
    }

    fn expect_literal(&mut self, lit: &str) -> Result<(), Kv3Error> {
        if self.src[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            Ok(())
        } else {
            Err(self.err(format!("expected '{lit}'")))
        }
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<(), Kv3Error> {
        let tok = self.read_token()?;
        if tok.eq_ignore_ascii_case(kw) {
            Ok(())
        } else {
            Err(self.err(format!("expected '{kw}', got '{tok}'")))
        }
    }

    fn read_token(&mut self) -> Result<std::string::String, Kv3Error> {
        self.skip_ws_and_comments()?;
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_whitespace() || "{}[]=, \t\n\r'\":|;".contains(c) {
                break;
            }
            self.bump();
        }
        if self.pos == start {
            return Err(self.err("expected a token"));
        }
        Ok(self.src[start..self.pos].to_string())
    }

    fn read_guid(&mut self) -> Result<Guid, Kv3Error> {
        let tok = self.read_token()?;
        Guid::from_text(&tok).ok_or_else(|| self.err(format!("invalid guid '{tok}'")))
    }

    fn parse_value(&mut self) -> Result<Value, Kv3Error> {
        self.skip_ws_and_comments()?;
        let mut flag = Flag::None;

        // A leading identifier immediately followed by ':' or '|' is a flag prefix; the last
        // one wins if there are several (FORMATS.md 4).
        loop {
            self.skip_ws_and_comments()?;
            match self.peek() {
                Some(c) if is_flag_start(c) => {
                    let save = self.pos;
                    if let Some(f) = self.try_read_flag()? {
                        flag = f;
                        continue;
                    }
                    self.pos = save;
                    break;
                }
                _ => break,
            }
        }

        self.skip_ws_and_comments()?;
        let value = match self.peek() {
            Some('{') => self.parse_object()?,
            Some('[') => self.parse_array()?,
            Some('#') => self.parse_blob()?,
            Some('"') | Some('\'') => Value::String(self.read_quoted_string()?),
            Some(_) => {
                let tok = self.read_bare_token()?;
                parse_literal(&tok)
            }
            None => return Err(self.err("unexpected end of input, expected a value")),
        };

        Ok(Value::with_flag(flag, value))
    }

    fn try_read_flag(&mut self) -> Result<Option<Flag>, Kv3Error> {
        // Token scanning stops at the same terminators as any other bare token (":" and "|"
        // included); a token immediately followed by ':' or '|' is a flag (FORMATS.md 4).
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_whitespace() || "{}[]=, \t\n\r'\":|;".contains(c) {
                break;
            }
            self.bump();
        }
        let tok = &self.src[start..self.pos];
        if tok.is_empty() {
            return Ok(None);
        }
        match self.peek() {
            Some(':') | Some('|') => {
                let flag = parse_flag_name(tok);
                if flag.is_none() {
                    return Err(self.err(format!("unknown flag '{tok}'")));
                }
                self.bump();
                Ok(flag)
            }
            _ => Ok(None),
        }
    }

    fn read_bare_token(&mut self) -> Result<std::string::String, Kv3Error> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_whitespace() || "{}[]=, \t\n\r'\":|;".contains(c) {
                break;
            }
            self.bump();
        }
        if self.pos == start {
            return Err(self.err("expected a value"));
        }
        Ok(self.src[start..self.pos].to_string())
    }

    fn read_quoted_string(&mut self) -> Result<std::string::String, Kv3Error> {
        let quote = self.peek().unwrap();
        self.bump();

        if quote == '"' && self.peek() == Some('"') {
            self.bump();
            if self.peek() == Some('"') {
                // Multiline string """..."""
                self.bump();
                if self.peek() == Some('\r') {
                    self.bump();
                }
                self.expect_char('\n')?;
                let start = self.pos;
                loop {
                    if self.eof() {
                        return Err(self.err("unterminated multiline string"));
                    }
                    if self.src[self.pos..].starts_with("\"\"\"") {
                        let mut s = self.src[start..self.pos].to_string();
                        if s.ends_with('\n') {
                            s.pop();
                            if s.ends_with('\r') {
                                s.pop();
                            }
                        }
                        self.pos += 3;
                        return Ok(s);
                    }
                    self.bump();
                }
            } else {
                // Empty string ""
                return Ok(std::string::String::new());
            }
        }

        let mut out = std::string::String::new();
        loop {
            let c = self.bump().ok_or_else(|| self.err("unterminated string"))?;
            if c == '\\' {
                let esc = self.bump().ok_or_else(|| self.err("unterminated escape"))?;
                match esc {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    other => out.push(other),
                }
            } else if c == quote {
                break;
            } else {
                out.push(c);
            }
        }
        Ok(out)
    }

    fn parse_blob(&mut self) -> Result<Value, Kv3Error> {
        self.expect_char('#')?;
        self.expect_char('[')?;
        // Collect strictly-hexdigit nibble values as we go, rather than building a `String` and
        // slicing it by byte offset afterwards: a non-ASCII character (e.g. `é`) has the same
        // *character* count as any other but a different *byte* length, so byte-offset slicing
        // could land inside it and panic on `str::from_utf8`. `char::to_digit(16)` also rejects
        // a leading sign, unlike `u8::from_str_radix`, which would otherwise silently accept
        // e.g. `"+f"` as a valid hex pair.
        let mut nibbles: Vec<u8> = Vec::new();
        loop {
            let c = self.bump().ok_or_else(|| self.err("unterminated blob"))?;
            if c == ']' {
                break;
            }
            if c.is_whitespace() {
                continue;
            }
            let digit = c
                .to_digit(16)
                .ok_or_else(|| self.err("invalid hex digit in blob"))?;
            nibbles.push(digit as u8);
        }
        if !nibbles.len().is_multiple_of(2) {
            return Err(self.err("blob hex string has odd length"));
        }
        let bytes = nibbles
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| (pair[0] << 4) | pair[1])
            .collect();
        Ok(Value::Blob(bytes))
    }

    fn parse_array(&mut self) -> Result<Value, Kv3Error> {
        self.expect_char('[')?;
        self.enter_depth()?;
        let mut items = Vec::new();
        loop {
            self.skip_ws_and_comments()?;
            if self.peek() == Some(']') {
                self.bump();
                break;
            }
            let value = self.parse_value()?;
            items.push(value);
            self.skip_ws_and_comments()?;
            if self.peek() == Some(',') {
                self.bump();
            }
        }
        self.leave_depth();
        Ok(Value::Array(items))
    }

    fn parse_object(&mut self) -> Result<Value, Kv3Error> {
        self.expect_char('{')?;
        self.enter_depth()?;
        let mut obj = Object::new();
        loop {
            self.skip_ws_and_comments()?;
            if self.peek() == Some('}') {
                self.bump();
                break;
            }
            let key = self.read_key()?;
            self.skip_ws_and_comments()?;
            if self.peek() == Some('=') {
                self.bump();
            }
            let value = self.parse_value()?;
            obj.push(key, value);
            self.skip_ws_and_comments()?;
            if self.peek() == Some(',') {
                self.bump();
            }
        }
        self.leave_depth();
        Ok(Value::Object(obj))
    }

    fn read_key(&mut self) -> Result<std::string::String, Kv3Error> {
        self.skip_ws_and_comments()?;
        match self.peek() {
            Some('"') | Some('\'') => self.read_quoted_string(),
            _ => self.read_bare_token(),
        }
    }
}

fn is_flag_start(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn parse_flag_name(name: &str) -> Option<Flag> {
    if name.eq_ignore_ascii_case("resource") {
        Some(Flag::Resource)
    } else if name.eq_ignore_ascii_case("resource_name") {
        Some(Flag::ResourceName)
    } else if name.eq_ignore_ascii_case("panorama") {
        Some(Flag::Panorama)
    } else if name.eq_ignore_ascii_case("soundevent") {
        Some(Flag::SoundEvent)
    } else if name.eq_ignore_ascii_case("subclass") {
        Some(Flag::SubClass)
    } else if name.eq_ignore_ascii_case("entity_name") {
        Some(Flag::EntityName)
    } else {
        None
    }
}

fn parse_literal(text: &str) -> Value {
    if text == "false" {
        return Value::Bool(false);
    }
    if text == "true" {
        return Value::Bool(true);
    }
    if text == "null" {
        return Value::Null;
    }
    if text.eq_ignore_ascii_case("nan") {
        return Value::Double(f64::NAN);
    }
    if text.eq_ignore_ascii_case("inf") || text.eq_ignore_ascii_case("+inf") {
        return Value::Double(f64::INFINITY);
    }
    if text.eq_ignore_ascii_case("-inf") {
        return Value::Double(f64::NEG_INFINITY);
    }

    let first = text.chars().next();
    let looks_numeric = matches!(first, Some(c) if c.is_ascii_digit() || c == '-' || c == '+');
    if looks_numeric {
        if text.starts_with('-') {
            if let Ok(i) = text.parse::<i64>() {
                return Value::Int(i);
            }
        } else if let Ok(u) = text.parse::<u64>() {
            return Value::UInt(u);
        }
        if let Ok(f) = text.parse::<f64>() {
            // Reject values Rust parses but which aren't ordinary decimal numbers here (e.g.
            // "inf"/"nan" spellings are handled above already; f64::from_str also accepts
            // those spellings, but we already returned before reaching here for them).
            return Value::Double(f);
        }
    }

    Value::String(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(root: Value) -> Document {
        Document {
            format: FORMAT_GENERIC,
            encoding: Some(ENCODING_TEXT),
            version: Kv3Version::Text,
            root,
        }
    }

    #[test]
    fn depth_at_limit_does_not_overflow_a_4mib_stack() {
        let n = TEXT_RECURSION_LIMIT as usize;
        let src = format!("{HEADER}{}{}", "[".repeat(n), "]".repeat(n));
        let handle = std::thread::Builder::new()
            .stack_size(4 << 20)
            .spawn(move || parse_text(&src).map(|_| ()))
            .expect("spawn probe thread");
        let result = handle
            .join()
            .expect("parser thread must not overflow the stack");
        assert!(
            result.is_ok(),
            "depth {n} (== TEXT_RECURSION_LIMIT) should parse successfully: {result:?}"
        );
    }

    #[test]
    fn depth_over_limit_errors_without_overflow() {
        let n = TEXT_RECURSION_LIMIT as usize + 50;
        let src = format!("{HEADER}{}{}", "[".repeat(n), "]".repeat(n));
        let handle = std::thread::Builder::new()
            .stack_size(4 << 20)
            .spawn(move || parse_text(&src).err().map(|e| e.to_string()))
            .expect("spawn probe thread");
        let err = handle
            .join()
            .expect("parser thread must not overflow the stack");
        assert!(
            matches!(err, Some(ref msg) if msg.contains("recursion limit")),
            "expected a recursion limit error, got {err:?}"
        );
    }

    #[test]
    fn very_deep_input_errors_without_overflow() {
        // A much larger nesting count than the limit (matching the review's original 100k-`[`
        // reproducer) must still bail out quickly with an error, not overflow the stack.
        let src = format!("{HEADER}{}", "[".repeat(100_000));
        let handle = std::thread::Builder::new()
            .stack_size(4 << 20)
            .spawn(move || parse_text(&src).is_err())
            .expect("spawn probe thread");
        let result = handle
            .join()
            .expect("parser thread must not overflow the stack");
        assert!(result);
    }

    const HEADER: &str = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n";

    #[test]
    fn parses_minimal_document() {
        let src = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n{\n}\n";
        let d = parse_text(src).unwrap();
        assert_eq!(d.root, Value::Object(Object::new()));
    }

    #[test]
    fn roundtrips_scalars_and_containers() {
        // Note: the text grammar cannot distinguish signed from unsigned for non-negative
        // integers (FORMATS.md 4 / KV3TextReader.ParseValue tries ulong before long for
        // non-negative tokens), so a positive `Value::Int` does not round-trip as `Int`; this
        // test uses negative values where the case matters.
        let mut obj = Object::new();
        obj.push("a", Value::Int(-1));
        obj.push("b", Value::UInt(2));
        obj.push("c", Value::Double(1.5));
        obj.push("d", Value::String("hello world".to_string()));
        obj.push("e", Value::Bool(true));
        obj.push("f", Value::Null);
        obj.push("g", Value::Array(vec![Value::Int(-1), Value::Int(-2)]));
        obj.push("h", Value::Blob(vec![0xDE, 0xAD, 0xBE, 0xEF]));
        obj.push(
            "i",
            Value::with_flag(Flag::Resource, Value::String("models/foo.vmdl".to_string())),
        );

        let d = doc(Value::Object(obj));
        let text = to_text(&d);
        let back = parse_text(&text).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn parses_flags_and_flag_precedence() {
        let src = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n{ a = resource:panorama:\"x\" }\n";
        let d = parse_text(src).unwrap();
        let v = d.root.get("a").unwrap();
        assert_eq!(v.flag(), Flag::Panorama);
    }

    #[test]
    fn parses_comments() {
        let src = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n{ // comment\n a = 1 /* block */\n}\n";
        let d = parse_text(src).unwrap();
        assert_eq!(d.root.get("a"), Some(&Value::UInt(1)));
    }

    #[test]
    fn parses_multiline_string() {
        let src = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n{ a = \"\"\"\nhello\nworld\n\"\"\" }\n";
        let d = parse_text(src).unwrap();
        assert_eq!(d.root.get("a").unwrap().as_str(), Some("hello\nworld"));
    }

    #[test]
    fn blob_with_non_ascii_char_errors_without_panic() {
        let src = format!("{HEADER}{{ a = #[a\u{e9}b] }}");
        assert!(parse_text(&src).is_err());
    }

    #[test]
    fn blob_rejects_signed_hex_pairs() {
        let src = format!("{HEADER}{{ a = #[+F+F] }}");
        assert!(parse_text(&src).is_err());
    }

    #[test]
    fn roundtrips_string_containing_triple_quote() {
        let mut o = Object::new();
        o.push("a", Value::String("a\n\"\"\"b".to_string()));
        let d = doc(Value::Object(o));
        let text = to_text(&d);
        assert!(
            !text.contains("\"\"\"\na\n\"\"\"b"),
            "must not use the multiline form: {text}"
        );
        let back = parse_text(&text).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn roundtrips_string_ending_in_cr() {
        let mut o = Object::new();
        o.push("a", Value::String("x\ny\r".to_string()));
        let d = doc(Value::Object(o));
        let text = to_text(&d);
        let back = parse_text(&text).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn blob_parses_valid_hex() {
        let src = format!("{HEADER}{{ a = #[DE AD BE EF] }}");
        let d = parse_text(&src).unwrap();
        assert_eq!(
            d.root.get("a").unwrap().as_blob(),
            Some(&[0xDE, 0xAD, 0xBE, 0xEF][..])
        );
    }
}
