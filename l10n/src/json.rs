//! The JSON the message files need, with no dependency: objects keep their order, a repeated
//! key is an error, and `pretty` writes the layout Xcode gives a String Catalog.

use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        self.as_obj()?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_obj(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Obj(o) => Some(o),
            _ => None,
        }
    }

    /// A list of strings, or `None` if this is anything else.
    pub fn as_strs(&self) -> Option<Vec<String>> {
        self.as_arr()?
            .iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect()
    }
}

pub fn parse(src: &str) -> Result<Json, String> {
    let mut p = Parser {
        s: src.as_bytes(),
        i: 0,
    };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.i < p.s.len() {
        return Err(p.err("text after the JSON value"));
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn err(&self, what: &str) -> String {
        let before = &self.s[..self.i.min(self.s.len())];
        let line = before.iter().filter(|&&b| b == b'\n').count() + 1;
        let col = before.iter().rev().take_while(|&&b| b != b'\n').count() + 1;
        format!("line {line}, column {col}: {what}")
    }

    fn ws(&mut self) {
        while matches!(self.s.get(self.i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, lit: &str) -> bool {
        let hit = self.s[self.i..].starts_with(lit.as_bytes());
        if hit {
            self.i += lit.len();
        }
        hit
    }

    fn value(&mut self) -> Result<Json, String> {
        match self.s.get(self.i) {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string().map(Json::Str),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ if self.eat("true") => Ok(Json::Bool(true)),
            _ if self.eat("false") => Ok(Json::Bool(false)),
            _ if self.eat("null") => Ok(Json::Null),
            _ => Err(self.err("expected a JSON value")),
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.i += 1;
        let mut out: Vec<(String, Json)> = Vec::new();
        self.ws();
        if self.eat("}") {
            return Ok(Json::Obj(out));
        }
        loop {
            self.ws();
            if self.s.get(self.i) != Some(&b'"') {
                return Err(self.err("expected a key"));
            }
            let at = self.i;
            let key = self.string()?;
            if out.iter().any(|(k, _)| *k == key) {
                self.i = at;
                return Err(self.err(&format!("key \"{key}\" appears twice")));
            }
            self.ws();
            if !self.eat(":") {
                return Err(self.err("expected ':'"));
            }
            self.ws();
            out.push((key, self.value()?));
            self.ws();
            if self.eat("}") {
                return Ok(Json::Obj(out));
            }
            if !self.eat(",") {
                return Err(self.err("expected ',' or '}'"));
            }
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.i += 1;
        let mut out = Vec::new();
        self.ws();
        if self.eat("]") {
            return Ok(Json::Arr(out));
        }
        loop {
            self.ws();
            out.push(self.value()?);
            self.ws();
            if self.eat("]") {
                return Ok(Json::Arr(out));
            }
            if !self.eat(",") {
                return Err(self.err("expected ',' or ']'"));
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.i += 1;
        let mut out = Vec::new();
        loop {
            let Some(&b) = self.s.get(self.i) else {
                return Err(self.err("unterminated string"));
            };
            self.i += 1;
            match b {
                b'"' => return String::from_utf8(out).map_err(|_| self.err("invalid UTF-8")),
                b'\\' => {
                    let esc = self.s.get(self.i).copied();
                    self.i += 1;
                    let c = match esc {
                        Some(b'"') => '"',
                        Some(b'\\') => '\\',
                        Some(b'/') => '/',
                        Some(b'b') => '\u{8}',
                        Some(b'f') => '\u{c}',
                        Some(b'n') => '\n',
                        Some(b'r') => '\r',
                        Some(b't') => '\t',
                        Some(b'u') => self.unicode()?,
                        _ => return Err(self.err("unknown escape")),
                    };
                    let mut buf = [0; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
                0..0x20 => return Err(self.err("control character in a string")),
                _ => out.push(b),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self
            .s
            .get(self.i..self.i + 4)
            .ok_or_else(|| self.err("short \\u escape"))?;
        let text = std::str::from_utf8(digits).map_err(|_| self.err("bad \\u escape"))?;
        let v = u32::from_str_radix(text, 16).map_err(|_| self.err("bad \\u escape"))?;
        self.i += 4;
        Ok(v)
    }

    fn unicode(&mut self) -> Result<char, String> {
        let hi = self.hex4()?;
        let code = if (0xD800..0xDC00).contains(&hi) {
            if !self.eat("\\u") {
                return Err(self.err("lone surrogate"));
            }
            let lo = self.hex4()?;
            if !(0xDC00..0xE000).contains(&lo) {
                return Err(self.err("lone surrogate"));
            }
            0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
        } else {
            hi
        };
        char::from_u32(code).ok_or_else(|| self.err("lone surrogate"))
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        while matches!(
            self.s.get(self.i),
            Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
        ) {
            self.i += 1;
        }
        let text = std::str::from_utf8(&self.s[start..self.i]).unwrap_or("");
        text.parse()
            .map(Json::Num)
            .map_err(|_| self.err("bad number"))
    }
}

/// `s` as a JSON string literal.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Two-space indent and `" : "` between key and value, as Xcode writes `.xcstrings`.
pub fn pretty(v: &Json) -> String {
    let mut out = String::new();
    write_pretty(v, 0, &mut out);
    out.push('\n');
    out
}

fn write_pretty(v: &Json, depth: usize, out: &mut String) {
    let pad = |out: &mut String, d: usize| out.push_str(&"  ".repeat(d));
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Num(n) => {
            let _ = write!(out, "{n}");
        }
        Json::Str(s) => out.push_str(&quote(s)),
        Json::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                out.push_str(if i == 0 { "\n" } else { ",\n" });
                pad(out, depth + 1);
                write_pretty(item, depth + 1, out);
            }
            out.push('\n');
            pad(out, depth);
            out.push(']');
        }
        Json::Obj(fields) => {
            out.push('{');
            for (i, (k, item)) in fields.iter().enumerate() {
                out.push_str(if i == 0 { "\n" } else { ",\n" });
                pad(out, depth + 1);
                out.push_str(&quote(k));
                out.push_str(" : ");
                write_pretty(item, depth + 1, out);
            }
            out.push('\n');
            pad(out, depth);
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, pretty, quote, Json};

    #[test]
    fn parses_what_message_files_hold() {
        let v =
            parse(r#" {"a": "x\u00e9\"\\\n", "b": [1, -2.5e1, true, null], "c": {}} "#).unwrap();
        assert_eq!(v.get("a").and_then(Json::as_str), Some("xé\"\\\n"));
        let b = v.get("b").and_then(Json::as_arr).unwrap();
        assert_eq!(b[1].as_f64(), Some(-25.0));
        assert_eq!(b[2], Json::Bool(true));
        assert_eq!(v.get("c").and_then(Json::as_obj).map(<[_]>::len), Some(0));
        assert_eq!(parse(r#""\ud83d\ude00""#).unwrap(), Json::Str("😀".into()));
    }

    #[test]
    fn keeps_order_and_refuses_a_repeated_key() {
        let v = parse(r#"{"z": 1, "a": 2}"#).unwrap();
        let keys: Vec<&str> = v
            .as_obj()
            .unwrap()
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(keys, ["z", "a"]);
        let e = parse("{\"a\": 1,\n \"a\": 2}").unwrap_err();
        assert_eq!(e, "line 2, column 2: key \"a\" appears twice");
    }

    #[test]
    fn rejects_broken_input() {
        for bad in [
            r#"{"a": 1,}"#,
            r#"{"a" 1}"#,
            "[1 2]",
            "\"open",
            "{} x",
            "\"\\q\"",
            "\"\t\"",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn writes_xcode_layout() {
        let v = Json::Obj(vec![
            ("k".into(), Json::Str("a\"b\n".into())),
            ("o".into(), Json::Obj(vec![("n".into(), Json::Num(1.0))])),
        ]);
        assert_eq!(
            pretty(&v),
            "{\n  \"k\" : \"a\\\"b\\n\",\n  \"o\" : {\n    \"n\" : 1\n  }\n}\n"
        );
        assert_eq!(quote("\u{1}"), "\"\\u0001\"");
    }
}
