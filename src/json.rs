//! A minimal, dependency-free JSON reader/writer.
//!
//! fmt-rs only needs to read two strings out of the PreToolUse hook payload
//! (`tool_name` and `tool_input.command`) and emit a small fixed-shape
//! response. Pulling in a JSON crate would be overkill for that and works
//! against the "single static binary, zero dependencies" goal, so this module
//! implements just enough of RFC 8259: full value parsing with correct string
//! unescaping (including `\uXXXX` surrogate pairs) and string encoding.
//!
//! Anything malformed is a parse error; the hook treats every error as "emit
//! `{}` and leave the command untouched", so being strict here is safe.

/// A parsed JSON value. Numbers are kept as their raw text since fmt-rs never
/// inspects them.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    /// For an object, the value under `key`.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The string contents, if this is a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }
}

/// Parse a complete JSON document. Returns `Err(())` on any malformed input.
/// The unit error is intentional: the only caller maps every failure to the
/// same no-op response, so a richer error type would carry no information.
#[allow(clippy::result_unit_err)]
pub fn parse(input: &str) -> Result<Value, ()> {
    let mut p = JParser { b: input.as_bytes(), i: 0 };
    p.skip_ws();
    let v = p.value()?;
    p.skip_ws();
    if p.i != p.b.len() {
        return Err(()); // trailing garbage
    }
    Ok(v)
}

struct JParser<'a> {
    b: &'a [u8],
    i: usize,
}

impl JParser<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    fn value(&mut self) -> Result<Value, ()> {
        match self.peek().ok_or(())? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => Ok(Value::String(self.string()?)),
            b't' => self.literal("true", Value::Bool(true)),
            b'f' => self.literal("false", Value::Bool(false)),
            b'n' => self.literal("null", Value::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => Err(()),
        }
    }

    fn literal(&mut self, word: &str, val: Value) -> Result<Value, ()> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(val)
        } else {
            Err(())
        }
    }

    fn number(&mut self) -> Result<Value, ()> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        while let Some(c) = self.peek() {
            if matches!(c, b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
                self.i += 1;
            } else {
                break;
            }
        }
        if self.i == start {
            return Err(());
        }
        let s = core::str::from_utf8(&self.b[start..self.i]).map_err(|_| ())?;
        Ok(Value::Number(s.to_string()))
    }

    fn string(&mut self) -> Result<String, ()> {
        if self.peek() != Some(b'"') {
            return Err(());
        }
        self.i += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let c = self.peek().ok_or(())?;
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = self.peek().ok_or(())?;
                    self.i += 1;
                    match e {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0C),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let cp = self.unicode_escape()?;
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(cp.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return Err(()),
                    }
                }
                // A raw control character is invalid in a JSON string.
                0x00..=0x1F => return Err(()),
                // Any other byte (including UTF-8 continuation bytes) is copied
                // through verbatim.
                _ => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|_| ())
    }

    /// Parse the four hex digits after `\u`, combining surrogate pairs.
    fn unicode_escape(&mut self) -> Result<char, ()> {
        let hi = self.hex4()?;
        if (0xD800..=0xDBFF).contains(&hi) {
            // high surrogate: expect a `\uXXXX` low surrogate next
            if self.peek() == Some(b'\\') && self.b.get(self.i + 1) == Some(&b'u') {
                self.i += 2;
                let lo = self.hex4()?;
                if (0xDC00..=0xDFFF).contains(&lo) {
                    let c = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                    return char::from_u32(c).ok_or(());
                }
            }
            return Err(());
        }
        if (0xDC00..=0xDFFF).contains(&hi) {
            return Err(()); // lone low surrogate
        }
        char::from_u32(hi).ok_or(())
    }

    fn hex4(&mut self) -> Result<u32, ()> {
        let mut v = 0u32;
        for _ in 0..4 {
            let c = self.peek().ok_or(())?;
            let d = match c {
                b'0'..=b'9' => (c - b'0') as u32,
                b'a'..=b'f' => (c - b'a' + 10) as u32,
                b'A'..=b'F' => (c - b'A' + 10) as u32,
                _ => return Err(()),
            };
            v = v * 16 + d;
            self.i += 1;
        }
        Ok(v)
    }

    fn array(&mut self) -> Result<Value, ()> {
        self.i += 1; // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.peek().ok_or(())? {
                b',' => self.i += 1,
                b']' => {
                    self.i += 1;
                    break;
                }
                _ => return Err(()),
            }
        }
        Ok(Value::Array(items))
    }

    fn object(&mut self) -> Result<Value, ()> {
        self.i += 1; // '{'
        let mut entries = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Value::Object(entries));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(());
            }
            self.i += 1;
            self.skip_ws();
            let val = self.value()?;
            entries.push((key, val));
            self.skip_ws();
            match self.peek().ok_or(())? {
                b',' => self.i += 1,
                b'}' => {
                    self.i += 1;
                    break;
                }
                _ => return Err(()),
            }
        }
        Ok(Value::Object(entries))
    }
}

/// Encode a string as a JSON string literal (including the surrounding quotes).
pub fn encode_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_object() {
        let v = parse(r#"{"tool_name":"Bash","tool_input":{"command":"ls -la"}}"#).unwrap();
        assert_eq!(v.get("tool_name").and_then(|x| x.as_str()), Some("Bash"));
        assert_eq!(
            v.get("tool_input").and_then(|t| t.get("command")).and_then(|c| c.as_str()),
            Some("ls -la")
        );
    }

    #[test]
    fn decodes_string_escapes() {
        let v = parse(r#"{"c":"a\"b\\c\nd\tе"}"#).unwrap();
        assert_eq!(v.get("c").and_then(|x| x.as_str()), Some("a\"b\\c\nd\tе"));
    }

    #[test]
    fn decodes_unicode_and_surrogates() {
        let v = parse(r#"{"a":"é","b":"😀"}"#).unwrap();
        assert_eq!(v.get("a").and_then(|x| x.as_str()), Some("é"));
        assert_eq!(v.get("b").and_then(|x| x.as_str()), Some("😀"));
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse("{").is_err());
        assert!(parse(r#"{"a":}"#).is_err());
        assert!(parse(r#"{"a":1} junk"#).is_err());
        assert!(parse(r#""\uD83D""#).is_err()); // lone surrogate
    }

    #[test]
    fn ignores_extra_fields_and_whitespace() {
        let v = parse("{ \"tool_name\" : \"Bash\" , \"n\": 5, \"ok\": true }").unwrap();
        assert_eq!(v.get("tool_name").and_then(|x| x.as_str()), Some("Bash"));
    }

    #[test]
    fn round_trip_encode() {
        let s = "echo \"hi\"\n\tx=1";
        let json = format!(r#"{{"c":{}}}"#, encode_string(s));
        let v = parse(&json).unwrap();
        assert_eq!(v.get("c").and_then(|x| x.as_str()), Some(s));
    }
}
