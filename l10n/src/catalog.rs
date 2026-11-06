//! Reads an l10n directory: `locales.json`, `rules.json` and `messages/<part>/<lang>.json`,
//! parsed to the subset `SPEC.md` describes. build.rs and the `pf-l10n` binary both include
//! this file, so it uses nothing but `crate::json` and `crate::plural`.
//!
//! A problem is one line, `<path>: <key>: <reason>`, the path relative to the l10n directory.
//! A message that does not parse is left out and reported; the rest still load.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::json::{self, Json};
use crate::plural::Category;

#[derive(Clone, Debug, PartialEq)]
pub enum Piece {
    Text(String),
    Arg(String),
}

pub type Pattern = Vec<Piece>;

#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    Plain(Pattern),
    /// One plural selector over the input `count`; variants in file order.
    Plural {
        count: String,
        cases: Vec<(Category, Pattern)>,
    },
    /// A message of a TypeScript-only part: its placeholders, and each variant as written.
    Loose {
        names: BTreeSet<String>,
        texts: Vec<String>,
    },
}

impl Message {
    /// Every placeholder name, a plural's count included.
    pub fn placeholders(&self) -> BTreeSet<&str> {
        match self {
            Message::Plain(p) => names(p).collect(),
            Message::Plural { count, cases } => cases
                .iter()
                .flat_map(|(_, p)| names(p))
                .chain([count.as_str()])
                .collect(),
            Message::Loose { names, .. } => names.iter().map(String::as_str).collect(),
        }
    }

    /// The order of a generated function's arguments: as placeholders first appear in the
    /// text, a plural's `other` variant read first. A count no variant shows comes first.
    pub fn args(&self) -> Vec<String> {
        let mut out = Vec::new();
        match self {
            Message::Plain(p) => push_names(&mut out, p),
            Message::Plural { count, cases } => {
                let mut ordered: Vec<&(Category, Pattern)> = cases.iter().collect();
                ordered.sort_by_key(|(c, _)| (*c != Category::Other, *c));
                for (_, p) in ordered {
                    push_names(&mut out, p);
                }
                if !out.contains(count) {
                    out.insert(0, count.clone());
                }
            }
            Message::Loose { names, .. } => out.extend(names.iter().cloned()),
        }
        out
    }

    /// The count's input name, for a plural.
    pub fn count(&self) -> Option<&str> {
        match self {
            Message::Plural { count, .. } => Some(count),
            _ => None,
        }
    }

    /// Each variant as written, `{name}` for a placeholder: one for a plain message. A loose
    /// message's are the raw strings, escapes and markup kept.
    pub fn texts(&self) -> Vec<String> {
        match self {
            Message::Plain(p) => vec![display(p)],
            Message::Plural { cases, .. } => cases.iter().map(|(_, p)| display(p)).collect(),
            Message::Loose { texts, .. } => texts.clone(),
        }
    }
}

fn names(p: &Pattern) -> impl Iterator<Item = &str> {
    p.iter().filter_map(|piece| match piece {
        Piece::Arg(n) => Some(n.as_str()),
        Piece::Text(_) => None,
    })
}

fn push_names(out: &mut Vec<String>, p: &Pattern) {
    for n in names(p) {
        if !out.iter().any(|o| o == n) {
            out.push(n.to_owned());
        }
    }
}

/// The pattern as a person reads the file: escapes resolved, placeholders as `{name}`.
pub fn display(p: &Pattern) -> String {
    p.iter()
        .map(|piece| match piece {
            Piece::Text(t) => t.clone(),
            Piece::Arg(n) => format!("{{{n}}}"),
        })
        .collect()
}

/// `<area>_<thing>` in lowercase snake_case: one underscore at least, never two in a row.
pub fn valid_key(key: &str) -> bool {
    key.starts_with(|c: char| c.is_ascii_lowercase())
        && key.contains('_')
        && key.split('_').all(|w| {
            !w.is_empty()
                && w.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// A BCP-47 tag as a file name: `de`, `pt-BR`, `zh-Hant`.
pub fn valid_lang(tag: &str) -> bool {
    let mut subtags = tag.split('-');
    let primary = subtags.next().unwrap_or("");
    (2..=3).contains(&primary.len())
        && primary.bytes().all(|b| b.is_ascii_lowercase())
        && subtags
            .all(|s| (1..=8).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric()))
}

fn is_ident(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Words a placeholder cannot be named: it becomes a Rust and a Swift parameter.
const RESERVED: &[&str] = &[
    "abstract",
    "any",
    "as",
    "associatedtype",
    "async",
    "await",
    "become",
    "box",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "crate",
    "default",
    "defer",
    "deinit",
    "do",
    "dyn",
    "else",
    "enum",
    "extension",
    "extern",
    "fallthrough",
    "false",
    "fileprivate",
    "final",
    "fn",
    "for",
    "func",
    "gen",
    "guard",
    "if",
    "impl",
    "import",
    "in",
    "init",
    "inout",
    "internal",
    "is",
    "let",
    "loop",
    "macro",
    "match",
    "mod",
    "move",
    "mut",
    "nil",
    "open",
    "operator",
    "override",
    "precedencegroup",
    "priv",
    "private",
    "protocol",
    "pub",
    "public",
    "ref",
    "repeat",
    "rethrows",
    "return",
    "self",
    "some",
    "static",
    "struct",
    "subscript",
    "super",
    "switch",
    "throw",
    "throws",
    "trait",
    "true",
    "try",
    "type",
    "typealias",
    "typeof",
    "unsafe",
    "unsized",
    "use",
    "var",
    "virtual",
    "where",
    "while",
    "yield",
];

fn check_placeholder(name: &str) -> Result<(), String> {
    let why = if name.starts_with(['#', '/']) {
        "markup, which shared parts do not allow"
    } else if name.contains(':') {
        "a formatting function, which shared parts do not allow"
    } else if name.starts_with(|c: char| c.is_ascii_digit()) {
        "positional; name it after what it holds"
    } else if !is_ident(name) {
        "not a lowercase snake_case name"
    } else if RESERVED.contains(&name) {
        "a reserved word in Rust or Swift; name it after what it holds"
    } else {
        return Ok(());
    };
    Err(format!("{{{name}}} is {why}"))
}

/// A string of the shared subset: text, `{name}` placeholders, `\{` `\}` `\\` escapes.
pub fn parse_pattern(s: &str) -> Result<Pattern, String> {
    if s.is_empty() {
        return Err("the message is empty".into());
    }
    let mut out = Vec::new();
    let mut text = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(e @ ('{' | '}' | '\\')) => text.push(e),
                _ => return Err(r"a backslash escapes only {, } or \; write \\ for one".into()),
            },
            '{' => {
                let mut name = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(c) => name.push(c),
                        None => {
                            return Err(r"a { is never closed; write \{ for a literal brace".into())
                        }
                    }
                }
                check_placeholder(&name)?;
                if !text.is_empty() {
                    out.push(Piece::Text(std::mem::take(&mut text)));
                }
                out.push(Piece::Arg(name));
            }
            '}' => return Err(r"a } closes nothing; write \} for a literal brace".into()),
            c => text.push(c),
        }
    }
    if !text.is_empty() {
        out.push(Piece::Text(text));
    }
    Ok(out)
}

const SHAPE: &str =
    "a message is a string, or a one-element array of declarations, selectors and match";

/// One message value. `loose` reads a TypeScript-only part: its placeholders and raw texts.
pub fn parse_message(v: &Json, loose: bool) -> Result<Message, String> {
    if loose {
        return loose_message(v);
    }
    if let Json::Str(s) = v {
        return parse_pattern(s).map(Message::Plain);
    }
    let Some([obj]) = v.as_arr() else {
        return Err(SHAPE.into());
    };
    let fields = obj.as_obj().ok_or(SHAPE)?;
    if let Some((k, _)) = fields
        .iter()
        .find(|(k, _)| !matches!(k.as_str(), "declarations" | "selectors" | "match"))
    {
        return Err(format!("unexpected field \"{k}\"; {SHAPE}"));
    }
    let decls = obj
        .get("declarations")
        .and_then(Json::as_strs)
        .ok_or("declarations must be a list of strings")?;
    let mut local = None;
    for d in &decls {
        if d.strip_prefix("input ").is_some_and(|n| is_ident(n.trim())) {
            continue;
        }
        match parse_local(d) {
            Some(l) if local.is_none() => local = Some(l),
            _ => {
                return Err(format!(
                    "declaration \"{d}\": the subset has inputs and one `local n = count: plural`"
                ));
            }
        }
    }
    let (sel, count) = local.ok_or("a plural needs `local n = count: plural`")?;
    check_placeholder(&count)?;
    let sels = obj.get("selectors").and_then(Json::as_strs);
    if sels.as_deref() != Some(std::slice::from_ref(&sel)) {
        return Err(format!(
            "selectors must be [\"{sel}\"]: one plural selector"
        ));
    }
    let arms = obj
        .get("match")
        .and_then(Json::as_obj)
        .filter(|m| !m.is_empty())
        .ok_or("match must be an object of variants")?;
    let mut cases: Vec<(Category, Pattern)> = Vec::new();
    for (k, text) in arms {
        let cat = k
            .split_once('=')
            .filter(|(s, _)| s.trim() == sel)
            .and_then(|(_, c)| match c.trim() {
                "*" => Some(Category::Other),
                c => Category::from_name(c),
            })
            .ok_or_else(|| format!("variant \"{k}\" is not {sel}=zero|one|two|few|many|other|*"))?;
        if cases.iter().any(|(c, _)| *c == cat) {
            return Err(format!("variant \"{k}\" repeats {}", cat.name()));
        }
        let text = text
            .as_str()
            .ok_or_else(|| format!("variant \"{k}\" is not a string"))?;
        let p = parse_pattern(text).map_err(|e| format!("variant \"{k}\": {e}"))?;
        if names(&p).any(|n| n == sel) {
            return Err(format!(
                "{{{sel}}} is the plural category; write {{{count}}}"
            ));
        }
        cases.push((cat, p));
    }
    Ok(Message::Plural { count, cases })
}

/// `local n = count: plural`, as `(n, count)`.
fn parse_local(d: &str) -> Option<(String, String)> {
    let (name, rhs) = d.strip_prefix("local ")?.split_once('=')?;
    let (input, func) = rhs.split_once(':')?;
    let (name, input) = (name.trim(), input.trim());
    (func.trim() == "plural" && is_ident(name) && is_ident(input))
        .then(|| (name.to_owned(), input.to_owned()))
}

/// Each variant's raw text, and its placeholder names as Paraglide reads them, markup and
/// functions allowed.
fn loose_message(v: &Json) -> Result<Message, String> {
    let mut texts = Vec::new();
    match v {
        Json::Str(s) => texts.push(s.clone()),
        Json::Arr(items) if !items.is_empty() => {
            for item in items {
                for (_, text) in item.get("match").and_then(Json::as_obj).ok_or(SHAPE)? {
                    texts.push(text.as_str().ok_or("a variant is not a string")?.to_owned());
                }
            }
        }
        _ => return Err(SHAPE.into()),
    }
    let mut names = BTreeSet::new();
    for t in &texts {
        scan(t, &mut names);
    }
    Ok(Message::Loose { names, texts })
}

fn scan(s: &str, out: &mut BTreeSet<String>) {
    let mut rest = s;
    while let Some(i) = rest.find(['\\', '{']) {
        let after = &rest[i + 1..];
        if rest[i..].starts_with('\\') {
            rest = &after[after.chars().next().map_or(0, char::len_utf8)..];
            continue;
        }
        let Some(end) = after.find('}') else { break };
        let inner = after[..end].trim();
        let name = inner
            .split(|c: char| c.is_whitespace() || c == ':')
            .next()
            .unwrap_or("");
        if !name.is_empty() && !inner.starts_with(['#', '/']) {
            out.insert(name.to_owned());
        }
        rest = &after[end + 1..];
    }
}

pub struct Entry {
    pub key: String,
    pub msg: Message,
}

pub struct File {
    pub lang: String,
    /// Relative to the l10n directory, `/`-separated.
    pub path: String,
    pub entries: Vec<Entry>,
}

impl File {
    pub fn get(&self, key: &str) -> Option<&Message> {
        self.entries.iter().find(|e| e.key == key).map(|e| &e.msg)
    }
}

pub struct Part {
    pub name: String,
    pub ts_only: bool,
    pub source: File,
    pub translations: Vec<File>,
    /// `notes.json`, or `None` when the part has none.
    pub notes: Option<Vec<(String, String)>>,
}

pub struct Catalog {
    pub root: PathBuf,
    pub source: String,
    /// The source first, then the rest in `locales.json` order.
    pub shipped: Vec<String>,
    pub fallback: Vec<(String, String)>,
    /// Parts compiled with no Cargo feature.
    pub always: Vec<String>,
    pub rules: Json,
    pub parts: Vec<Part>,
}

impl Catalog {
    pub fn part(&self, name: &str) -> Option<&Part> {
        self.parts.iter().find(|p| p.name == name)
    }

    /// The messages of `parts`, sorted by key, each compiled in every shipped language that
    /// has it. A translation that does not compile stops it with that problem's line.
    pub fn rows(&self, parts: &[String]) -> Result<Vec<Row<'_>>, String> {
        let mut rows = Vec::new();
        for name in parts {
            let part = self
                .part(name)
                .ok_or_else(|| format!("no part \"{name}\" in messages/"))?;
            if part.ts_only {
                return Err(format!("part \"{name}\" is TypeScript-only"));
            }
            for e in &part.source.entries {
                let mut langs = Vec::new();
                for lang in &self.shipped {
                    let file = if *lang == self.source {
                        Some(&part.source)
                    } else {
                        part.translations.iter().find(|f| f.lang == *lang)
                    };
                    let Some((path, msg)) = file.and_then(|f| Some((&f.path, f.get(&e.key)?)))
                    else {
                        continue;
                    };
                    let c =
                        compile(&e.msg, msg).map_err(|why| format!("{path}: {}: {why}", e.key))?;
                    langs.push((lang.as_str(), c));
                }
                let note = part.notes.iter().flatten().find(|(k, _)| *k == e.key);
                rows.push(Row {
                    key: &e.key,
                    en: &e.msg,
                    note: note.map(|(_, n)| n.as_str()),
                    langs,
                });
            }
        }
        rows.sort_by(|a, b| a.key.cmp(b.key));
        Ok(rows)
    }
}

/// One message, ready for a generator.
pub struct Row<'a> {
    pub key: &'a str,
    pub en: &'a Message,
    pub note: Option<&'a str>,
    /// The source language first, then each shipped language that has the message.
    pub langs: Vec<(&'a str, Compiled)>,
}

fn read_json(root: &Path, rel: &str) -> Result<Json, String> {
    let text = fs::read_to_string(root.join(rel)).map_err(|e| format!("{rel}: {e}"))?;
    json::parse(&text).map_err(|e| format!("{rel}: {e}"))
}

/// Reads `root`. Without `every_language` it reads what a build compiles: the parts outside
/// `typescript_only`, in the source and shipped languages.
pub fn load(root: &Path, every_language: bool) -> (Catalog, Vec<String>) {
    let mut problems = Vec::new();
    let mut cat = Catalog {
        root: root.to_owned(),
        source: "en".into(),
        shipped: vec!["en".into()],
        fallback: Vec::new(),
        always: Vec::new(),
        rules: Json::Null,
        parts: Vec::new(),
    };
    match read_json(root, "locales.json")
        .and_then(|v| locales(&v).map_err(|e| format!("locales.json: {e}")))
    {
        Ok((source, shipped, fallback)) => {
            (cat.source, cat.shipped, cat.fallback) = (source, shipped, fallback)
        }
        Err(e) => problems.push(e),
    }
    let mut ts_only = Vec::new();
    match read_json(root, "rules.json") {
        Ok(v) => {
            let list = |k: &str| {
                v.get("parts")
                    .and_then(|p| p.get(k))
                    .and_then(Json::as_strs)
            };
            match (list("always"), list("typescript_only")) {
                (Some(a), Some(t)) => (cat.always, ts_only) = (a, t),
                _ => problems
                    .push("rules.json: parts needs the lists always and typescript_only".into()),
            }
            cat.rules = v;
        }
        Err(e) => problems.push(e),
    }
    let mut names: Vec<String> = match fs::read_dir(root.join("messages")) {
        Ok(dir) => dir
            .filter_map(Result::ok)
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect(),
        Err(e) => {
            problems.push(format!("messages: {e}"));
            Vec::new()
        }
    };
    names.sort();
    for name in names {
        let ts = ts_only.contains(&name);
        if ts && !every_language {
            continue;
        }
        if !is_ident(&name) {
            problems.push(format!(
                "messages/{name}: a part's name is one lowercase word"
            ));
            continue;
        }
        let part = load_part(&cat, &name, ts, every_language, &mut problems);
        cat.parts.push(part);
    }
    (cat, problems)
}

type Locales = (String, Vec<String>, Vec<(String, String)>);

fn locales(v: &Json) -> Result<Locales, String> {
    let source = v
        .get("source")
        .and_then(Json::as_str)
        .ok_or("source must be a tag")?;
    let mut shipped = v
        .get("shipped")
        .and_then(Json::as_strs)
        .ok_or("shipped must be a list of tags")?;
    let fallback = v
        .get("fallback")
        .and_then(Json::as_obj)
        .ok_or("fallback must be an object")?
        .iter()
        .map(|(k, to)| to.as_str().map(|t| (k.clone(), t.to_owned())))
        .collect::<Option<Vec<_>>>()
        .ok_or("fallback maps a tag to a tag")?;
    if let Some(bad) = shipped
        .iter()
        .map(String::as_str)
        .chain([source])
        .find(|t| !valid_lang(t))
    {
        return Err(format!("\"{bad}\" is not a language tag"));
    }
    if let Some((from, to)) = fallback.iter().find(|(_, to)| !shipped.contains(to)) {
        return Err(format!(
            "fallback {from} -> {to} names a language that is not shipped"
        ));
    }
    shipped.retain(|l| l != source);
    shipped.insert(0, source.to_owned());
    Ok((source.to_owned(), shipped, fallback))
}

fn load_part(
    cat: &Catalog,
    name: &str,
    ts: bool,
    every_language: bool,
    problems: &mut Vec<String>,
) -> Part {
    let dir = format!("messages/{name}");
    let mut langs: Vec<String> = fs::read_dir(cat.root.join(&dir))
        .map(|d| {
            d.filter_map(Result::ok)
                .filter_map(|e| e.file_name().into_string().ok())
                .filter_map(|f| f.strip_suffix(".json").map(str::to_owned))
                .filter(|l| l != "notes" && *l != cat.source)
                .collect()
        })
        .unwrap_or_default();
    langs.sort();
    let mut file = |lang: &str| load_file(cat, &format!("{dir}/{lang}.json"), lang, ts, problems);
    let source = file(&cat.source);
    let translations = langs
        .iter()
        .filter(|l| every_language || cat.shipped.contains(l))
        .map(|l| file(l))
        .collect();
    let notes_path = format!("{dir}/notes.json");
    let notes = if cat.root.join(&notes_path).exists() {
        match read_json(&cat.root, &notes_path) {
            Ok(Json::Obj(fields)) => Some(
                fields
                    .into_iter()
                    .filter_map(|(k, v)| match v {
                        Json::Str(s) => Some((k, s)),
                        _ => {
                            problems.push(format!("{notes_path}: {k}: a note is one string"));
                            None
                        }
                    })
                    .collect(),
            ),
            Ok(_) => {
                problems.push(format!(
                    "{notes_path}: notes are one JSON object of strings"
                ));
                Some(Vec::new())
            }
            Err(e) => {
                problems.push(e);
                Some(Vec::new())
            }
        }
    } else {
        if !ts {
            problems.push(format!(
                "{notes_path}: missing; write {{}} for a part without notes"
            ));
        }
        None
    };
    Part {
        name: name.to_owned(),
        ts_only: ts,
        source,
        translations,
        notes,
    }
}

fn load_file(cat: &Catalog, rel: &str, lang: &str, ts: bool, problems: &mut Vec<String>) -> File {
    let mut file = File {
        lang: lang.to_owned(),
        path: rel.to_owned(),
        entries: Vec::new(),
    };
    if !valid_lang(lang) {
        problems.push(format!("{rel}: \"{lang}\" is not a language tag"));
        return file;
    }
    let v = match read_json(&cat.root, rel) {
        Ok(v) => v,
        Err(e) => {
            problems.push(e);
            return file;
        }
    };
    let Json::Obj(fields) = v else {
        problems.push(format!("{rel}: a message file is one JSON object"));
        return file;
    };
    for (key, value) in fields {
        if key == "$schema" {
            continue;
        }
        if !valid_key(&key) {
            problems.push(format!(
                "{rel}: {key}: not a key; write <area>_<thing> in lowercase snake_case"
            ));
            continue;
        }
        match parse_message(&value, ts) {
            Ok(msg) => file.entries.push(Entry { key, msg }),
            Err(e) => problems.push(format!("{rel}: {key}: {e}")),
        }
    }
    file
}

/// A pattern piece with its placeholder resolved to an argument index.
#[derive(Clone, Debug, PartialEq)]
pub enum Ix {
    Text(String),
    Arg(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Compiled {
    Plain(Vec<Ix>),
    Plural(Vec<(Category, Vec<Ix>)>),
}

/// `msg` with each placeholder replaced by its index in `en.args()`. A translation keeps the
/// English shape: plain stays plain, a plural stays a plural over the same count.
pub fn compile(en: &Message, msg: &Message) -> Result<Compiled, String> {
    let args = en.args();
    let index = |p: &Pattern| -> Result<Vec<Ix>, String> {
        p.iter()
            .map(|piece| match piece {
                Piece::Text(t) => Ok(Ix::Text(t.clone())),
                Piece::Arg(n) => args
                    .iter()
                    .position(|a| a == n)
                    .map(Ix::Arg)
                    .ok_or_else(|| format!("{{{n}}} is not in the English text")),
            })
            .collect()
    };
    match (en, msg) {
        (Message::Plain(_), Message::Plain(p)) => index(p).map(Compiled::Plain),
        (Message::Plural { count, .. }, Message::Plural { count: c, cases }) if count == c => cases
            .iter()
            .map(|(cat, p)| Ok((*cat, index(p)?)))
            .collect::<Result<_, String>>()
            .map(Compiled::Plural),
        (Message::Plural { count, .. }, _) => {
            Err(format!("must be a plural over {{{count}}}, as in English"))
        }
        _ => Err("must be a plain string, as in English".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plural(json: &str) -> Result<Message, String> {
        parse_message(&json::parse(json).unwrap(), false)
    }

    #[test]
    fn keys() {
        for good in ["common_cancel", "player_stats_scale_pct", "a_4k"] {
            assert!(valid_key(good), "{good}");
        }
        for bad in [
            "cancel",
            "Common_cancel",
            "common__x",
            "common_",
            "_x",
            "x-y_z",
            "4k_x",
        ] {
            assert!(!valid_key(bad), "{bad}");
        }
    }

    #[test]
    fn languages() {
        assert!(["en", "de-AT", "pt-BR", "zh-Hant", "es-419"]
            .iter()
            .all(|l| valid_lang(l)));
        assert!(!["EN", "e", "de_AT", "notes", "de-"]
            .iter()
            .any(|l| valid_lang(l)));
    }

    #[test]
    fn patterns() {
        let p = parse_pattern(r"Hold {keys} \{x\} \\ ok").unwrap();
        assert_eq!(
            p,
            [
                Piece::Text("Hold ".into()),
                Piece::Arg("keys".into()),
                Piece::Text(r" {x} \ ok".into())
            ]
        );
        for (bad, why) in [
            ("{0} hosts", "positional"),
            ("{count :number}", "formatting function"),
            ("{#b}bold{/b}", "markup"),
            ("{type} pad", "reserved"),
            ("{Name}", "snake_case"),
            ("a { b", "never closed"),
            ("a } b", "closes nothing"),
            (r"C:\path", "backslash"),
            ("", "empty"),
        ] {
            let e = parse_pattern(bad).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
    }

    #[test]
    fn plurals() {
        let m = plural(
            r#"[{"declarations": ["input count", "input host", "local n = count: plural"],
                "selectors": ["n"],
                "match": {"n=one": "{host} has {count} pad", "n=*": "{count} pads on {host}"}}]"#,
        )
        .unwrap();
        assert_eq!(m.count(), Some("count"));
        assert_eq!(m.args(), ["count", "host"]);
        let Message::Plural { cases, .. } = &m else {
            panic!()
        };
        assert_eq!(cases[1].0, Category::Other);
        for (bad, why) in [
            (
                r#"[{"declarations": ["local n = count: number"], "selectors": ["n"], "match": {"n=*": "x"}}]"#,
                "declaration",
            ),
            (
                r#"[{"declarations": ["local n = count: plural"], "selectors": ["n", "m"], "match": {"n=*": "x"}}]"#,
                "one plural selector",
            ),
            (
                r#"[{"declarations": ["local n = count: plural"], "selectors": ["n"], "match": {"n=lots": "x"}}]"#,
                "is not n=",
            ),
            (
                r#"[{"declarations": ["local n = count: plural"], "selectors": ["n"], "match": {"n=other": "x", "n=*": "y"}}]"#,
                "repeats other",
            ),
            (
                r#"[{"declarations": ["local n = count: plural"], "selectors": ["n"], "match": {"n=*": "{n} x"}}]"#,
                "plural category",
            ),
            (
                r#"[{"declarations": [], "selectors": [], "match": {}, "x": 1}]"#,
                "unexpected field",
            ),
            (r#"[1, 2]"#, "one-element array"),
        ] {
            let e = plural(bad).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
    }

    #[test]
    fn loose_reads_placeholders_and_texts() {
        let v = json::parse(r#""{#b}{name}{/b} has {count :number} \\{x}""#).unwrap();
        let m = parse_message(&v, true).unwrap();
        assert_eq!(
            m.placeholders().into_iter().collect::<Vec<_>>(),
            ["count", "name"]
        );
        assert_eq!(m.texts(), [r"{#b}{name}{/b} has {count :number} \{x}"]);
        let v = json::parse(r#"[{"match": {"n=one": "{count} pad", "n=*": "{count} pads"}}]"#);
        let m = parse_message(&v.unwrap(), true).unwrap();
        assert_eq!(m.texts(), ["{count} pad", "{count} pads"]);
    }

    #[test]
    fn compile_maps_names_and_keeps_shape() {
        let en = Message::Plain(parse_pattern("{a} then {b}").unwrap());
        let de = Message::Plain(parse_pattern("{b} vor {a}").unwrap());
        let Compiled::Plain(ix) = compile(&en, &de).unwrap() else {
            panic!()
        };
        assert_eq!(ix, [Ix::Arg(1), Ix::Text(" vor ".into()), Ix::Arg(0)]);
        let odd = Message::Plain(parse_pattern("{c}").unwrap());
        assert!(compile(&en, &odd).unwrap_err().contains("{c}"));
        let p = plural(r#"[{"declarations": ["local n = count: plural"], "selectors": ["n"], "match": {"n=*": "{count}"}}]"#).unwrap();
        assert!(compile(&p, &de).unwrap_err().contains("plural"));
    }
}
