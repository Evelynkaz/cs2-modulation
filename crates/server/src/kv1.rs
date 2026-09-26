//! A minimal KV1 ("VDF") text reader: quoted or unquoted keys/values, nested `{}` blocks, `//`
//! line comments - just enough to read `resource/overviews/<map>.txt` (`s6p_map_art.md`). Not a
//! general Source engine KV1 writer or validator; `crates/s2fmt` has no KV1 reader to reuse
//! (its `kv3` module is an unrelated, newer format).

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Block(Vec<(String, Value)>),
}

impl Value {
    /// This value's children, if it's a [`Value::Block`].
    pub fn as_block(&self) -> Option<&[(String, Value)]> {
        match self {
            Value::Block(pairs) => Some(pairs),
            Value::Str(_) => None,
        }
    }

    /// This value as a plain string, if it's a [`Value::Str`].
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            Value::Block(_) => None,
        }
    }

    /// The first child keyed `key`, matched case-insensitively (Source engine KV1 keys are
    /// conventionally case-insensitive).
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_block()?
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Kv1Error {
    #[error("unexpected end of input while {0}")]
    UnexpectedEof(&'static str),
    #[error("expected '{{' at character {0}")]
    ExpectedBlock(usize),
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn new(src: &str) -> Self {
        Parser {
            chars: src.chars().collect(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<char> {
        self.chars.get(self.pos + 1).copied()
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            while matches!(self.peek(), Some(c) if c.is_whitespace()) {
                self.pos += 1;
            }
            if self.peek() == Some('/') && self.peek2() == Some('/') {
                while !matches!(self.peek(), None | Some('\n')) {
                    self.pos += 1;
                }
                continue;
            }
            break;
        }
    }

    /// One token: a `"..."`-quoted string (`\"` honoured as an escaped quote), or a run of
    /// non-whitespace, non-brace characters up to the next `//` comment - `s6p_map_art.md`'s
    /// "unquoted tokens tolerated".
    fn token(&mut self) -> Result<String, Kv1Error> {
        match self.peek() {
            None => Err(Kv1Error::UnexpectedEof("reading a token")),
            Some('"') => {
                self.pos += 1;
                let mut out = String::new();
                loop {
                    match self.peek() {
                        None => return Err(Kv1Error::UnexpectedEof("reading a quoted string")),
                        Some('"') => {
                            self.pos += 1;
                            break;
                        }
                        Some('\\') if self.peek2() == Some('"') => {
                            out.push('"');
                            self.pos += 2;
                        }
                        Some(c) => {
                            out.push(c);
                            self.pos += 1;
                        }
                    }
                }
                Ok(out)
            }
            Some(_) => {
                let mut out = String::new();
                while let Some(c) = self.peek() {
                    if c.is_whitespace() || c == '{' || c == '}' {
                        break;
                    }
                    if c == '/' && self.peek2() == Some('/') {
                        break;
                    }
                    out.push(c);
                    self.pos += 1;
                }
                if out.is_empty() {
                    // A stray brace or comment marker where a token was expected - consumed so a
                    // malformed file can't spin the parser forever instead of just failing later.
                    self.pos += 1;
                }
                Ok(out)
            }
        }
    }

    fn parse_block(&mut self) -> Result<Value, Kv1Error> {
        self.skip_ws_and_comments();
        if self.peek() != Some('{') {
            return Err(Kv1Error::ExpectedBlock(self.pos));
        }
        self.pos += 1;
        let mut pairs = Vec::new();
        loop {
            self.skip_ws_and_comments();
            match self.peek() {
                None => return Err(Kv1Error::UnexpectedEof("reading a block")),
                Some('}') => {
                    self.pos += 1;
                    break;
                }
                Some(_) => {
                    let key = self.token()?;
                    self.skip_ws_and_comments();
                    let value = if self.peek() == Some('{') {
                        self.parse_block()?
                    } else {
                        Value::Str(self.token()?)
                    };
                    pairs.push((key, value));
                }
            }
        }
        Ok(Value::Block(pairs))
    }
}

/// Parses a KV1 document (`"<root key>" { ... }`), returning the root block's value. The root
/// key itself (e.g. `"de_nuke"`) is discarded - callers already know which map they asked for.
pub fn parse(src: &str) -> Result<Value, Kv1Error> {
    let mut p = Parser::new(src);
    p.skip_ws_and_comments();
    let _root_key = p.token()?;
    p.parse_block()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_blocks_and_comments() {
        let src = r#"
            // a leading comment
            "root"
            {
                "a" "1" // trailing comment
                "nested"
                {
                    "b" "2"
                }
            }
        "#;
        let root = parse(src).unwrap();
        assert_eq!(root.get("a").unwrap().as_str(), Some("1"));
        let nested = root.get("nested").unwrap();
        assert_eq!(nested.get("b").unwrap().as_str(), Some("2"));
    }

    #[test]
    fn keys_are_matched_case_insensitively() {
        let root = parse(r#""root" { "CTSpawn_x" "0.5" }"#).unwrap();
        assert_eq!(root.get("ctspawn_x").unwrap().as_str(), Some("0.5"));
    }

    #[test]
    fn unquoted_tokens_are_tolerated() {
        let root = parse("root { a 1 }").unwrap();
        assert_eq!(root.get("a").unwrap().as_str(), Some("1"));
    }

    /// The exact `de_nuke` overview text from `s6p_map_art.md`.
    #[test]
    fn parses_de_nuke_overview_verbatim() {
        let src = r#"
// HLTV overview description file for de_nuke.bsp

"de_nuke"
{
	"material"	"overviews/de_nuke"	// texture file
	"pos_x"		"-3453"	// upper left world coordinate
	"pos_y"		"2887"
	"scale"		"7"

	"verticalsections"
	{
		"default" // use the primary radar image
		{
			"AltitudeMax" "10000"
			"AltitudeMin" "-495"
		}
		"lower" // i.e. de_nuke_lower_radar.dds
		{
			"AltitudeMax" "-495"
			"AltitudeMin" "-10000"
		}
	}

	// loading screen icons and positions
	"CTSpawn_x"	"0.82"
	"CTSpawn_y"	"0.45"
	"TSpawn_x"	"0.19"
	"TSpawn_y"	"0.54"

	"bombA_x"	"0.58"
	"bombA_y"	"0.48"
	"bombB_x"	"0.58"
	"bombB_y"	"0.58"

	"inset_left"		"0.33"
	"inset_top"			"0.2"
	"inset_right"		"0.2"
	"inset_bottom"		"0.2"

}
"#;
        let root = parse(src).unwrap();
        assert_eq!(root.get("pos_x").unwrap().as_str(), Some("-3453"));
        assert_eq!(root.get("pos_y").unwrap().as_str(), Some("2887"));
        assert_eq!(root.get("scale").unwrap().as_str(), Some("7"));
        let sections = root.get("verticalsections").unwrap();
        let default = sections.get("default").unwrap();
        assert_eq!(default.get("AltitudeMax").unwrap().as_str(), Some("10000"));
        assert_eq!(default.get("AltitudeMin").unwrap().as_str(), Some("-495"));
        let lower = sections.get("lower").unwrap();
        assert_eq!(lower.get("AltitudeMax").unwrap().as_str(), Some("-495"));
        assert_eq!(lower.get("AltitudeMin").unwrap().as_str(), Some("-10000"));
        assert_eq!(root.get("CTSpawn_x").unwrap().as_str(), Some("0.82"));
        assert_eq!(root.get("bombB_y").unwrap().as_str(), Some("0.58"));
    }
}
