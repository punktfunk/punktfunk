//! `pf-l10n check`: the rules on top of the format. Each rule is a small function over one
//! message, file or list, tested on its own; [`run`] applies them all to an l10n directory.
//! TypeScript-only parts get the key and placeholder rules only; Paraglide owns the rest.

use std::collections::BTreeMap;
use std::path::Path;

use crate::catalog::{self, Catalog, File, Message, Part};
use crate::json::{self, Json};
use crate::plural;

pub struct Rules {
    roles: Vec<(String, usize)>,
    label: usize,
    slack: f64,
    banned: Vec<Banned>,
    retired: Vec<(String, String)>,
    debt: Vec<String>,
}

struct Banned {
    text: String,
    at_end: bool,
    why: String,
}

impl Rules {
    pub fn from_json(v: &Json) -> Result<Rules, String> {
        let field = |v: &Json, k: &str| v.get(k).and_then(Json::as_str).map(str::to_owned);
        let b = v.get("budgets").ok_or("budgets is missing")?;
        let roles = b
            .get("roles")
            .and_then(Json::as_obj)
            .and_then(|o| {
                o.iter()
                    .map(|(k, n)| Some((k.clone(), n.as_f64()? as usize)))
                    .collect()
            })
            .ok_or("budgets.roles maps a key suffix to a length")?;
        let label = b
            .get("label")
            .and_then(Json::as_f64)
            .ok_or("budgets.label is a length")?;
        let slack = b
            .get("translation_slack")
            .and_then(Json::as_f64)
            .ok_or("budgets.translation_slack is a factor")?;
        let list = |k: &str| v.get(k).and_then(Json::as_arr).unwrap_or_default();
        let banned = list("banned")
            .iter()
            .map(|e| {
                let at_end = match field(e, "at")?.as_str() {
                    "end" => true,
                    "anywhere" => false,
                    _ => return None,
                };
                Some(Banned {
                    text: field(e, "text")?,
                    at_end,
                    why: field(e, "why")?,
                })
            })
            .collect::<Option<_>>()
            .ok_or("banned holds {text, at: anywhere|end, why}")?;
        let retired = list("retired")
            .iter()
            .map(|e| Some((field(e, "term")?, field(e, "use")?)))
            .collect::<Option<_>>()
            .ok_or("retired holds {term, use}")?;
        let debt = v
            .get("debt")
            .and_then(Json::as_obj)
            .ok_or("debt maps a key to its reason")?
            .iter()
            .map(|(k, _)| k.clone())
            .collect();
        Ok(Rules {
            roles,
            label: label as usize,
            slack,
            banned,
            retired,
            debt,
        })
    }
}

/// A key's length budget: by its role suffix, or the label's for a key without one.
fn budget(rules: &Rules, key: &str, source: bool) -> usize {
    let base = rules
        .roles
        .iter()
        .find(|(suffix, _)| key.ends_with(suffix.as_str()))
        .map_or(rules.label, |r| r.1);
    if source {
        base
    } else {
        (base as f64 * rules.slack).round() as usize
    }
}

fn over_budget(rules: &Rules, key: &str, text: &str, source: bool) -> Option<String> {
    let (len, max) = (text.chars().count(), budget(rules, key, source));
    (len > max).then(|| format!("{len} characters, over the budget of {max}"))
}

/// `needle` in `hay` as a whole word or phrase, ignoring case.
fn contains_word(hay: &str, needle: &str) -> bool {
    let (hay, needle) = (hay.to_lowercase(), needle.to_lowercase());
    hay.match_indices(&needle).any(|(i, m)| {
        let before = hay[..i].chars().next_back();
        let after = hay[i + m.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

fn banned(rules: &Rules, text: &str) -> Option<String> {
    let hit = rules.banned.iter().find(|b| {
        if b.at_end {
            text.trim_end().ends_with(b.text.as_str())
        } else {
            contains_word(text, &b.text)
        }
    })?;
    Some(format!("\"{}\": {}", hit.text, hit.why))
}

fn retired(rules: &Rules, text: &str) -> Option<String> {
    let (term, instead) = rules
        .retired
        .iter()
        .find(|(term, _)| contains_word(text, term))?;
    Some(format!("\"{term}\" is retired; write \"{instead}\""))
}

/// A plural's variants against the categories `lang` needs. Silent for a language without a
/// rule: `shipped_rules` reports that where it matters.
fn categories(lang: &str, msg: &Message) -> Option<String> {
    let Message::Plural { cases, .. } = msg else {
        return None;
    };
    let need = plural::categories(lang)?;
    let mut have: Vec<plural::Category> = cases.iter().map(|(c, _)| *c).collect();
    have.sort();
    let names =
        |cs: &[plural::Category]| cs.iter().map(|c| c.name()).collect::<Vec<_>>().join(", ");
    (have != need).then(|| {
        format!(
            "{lang} plurals need {}; this has {}",
            names(need),
            names(&have)
        )
    })
}

/// A translation's placeholders against the source's.
fn placeholders(source: &Message, tr: &Message) -> Option<String> {
    let (want, have) = (source.placeholders(), tr.placeholders());
    let list = |s: &std::collections::BTreeSet<&str>| {
        s.iter()
            .map(|p| format!("{{{p}}}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    (want != have).then(|| {
        format!(
            "placeholders are {}, the source has {}",
            or_none(list(&have)),
            or_none(list(&want))
        )
    })
}

fn or_none(s: String) -> String {
    if s.is_empty() {
        "none".into()
    } else {
        s
    }
}

/// Keys a translation has and the source does not.
fn orphans(source: &File, tr: &File) -> Vec<String> {
    tr.entries
        .iter()
        .filter(|e| source.get(&e.key).is_none())
        .map(|e| format!("{}: {}: not in {}", tr.path, e.key, source.path))
        .collect()
}

/// A key in more than one part.
fn duplicates(parts: &[Part]) -> Vec<String> {
    let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
    let mut out = Vec::new();
    for part in parts {
        for e in &part.source.entries {
            match seen.get(e.key.as_str()) {
                Some(first) => {
                    out.push(format!("{}: {}: also in {first}", part.source.path, e.key))
                }
                None => {
                    seen.insert(&e.key, &part.source.path);
                }
            }
        }
    }
    out
}

/// Notes for keys the part lacks, and messages with placeholders that have no note.
fn notes(part: &Part) -> Vec<String> {
    let Some(notes) = &part.notes else {
        return Vec::new();
    };
    let path = format!("messages/{}/notes.json", part.name);
    let stale = notes
        .iter()
        .filter(|(k, _)| part.source.get(k).is_none())
        .map(|(k, _)| format!("{path}: {k}: no message has this key"));
    let lacking = part
        .source
        .entries
        .iter()
        .filter(|e| !e.msg.placeholders().is_empty() && !notes.iter().any(|(k, _)| *k == e.key))
        .map(|e| format!("{path}: {}: has placeholders, so it needs a note", e.key));
    stale.chain(lacking).collect()
}

fn shipped_rules(shipped: &[String]) -> Vec<String> {
    shipped
        .iter()
        .filter(|l| plural::categories(l).is_none())
        .map(|l| format!("locales.json: {l} ships without a plural rule in src/plural.rs"))
        .collect()
}

fn stale_debt(rules: &Rules, parts: &[Part]) -> Vec<String> {
    rules
        .debt
        .iter()
        .filter(|k| !parts.iter().any(|p| p.source.get(k).is_some()))
        .map(|k| format!("rules.json: debt: {k}: no part has this key"))
        .collect()
}

fn glossary(root: &Path) -> Vec<String> {
    let v = match std::fs::read_to_string(root.join("glossary.json")) {
        Ok(text) => json::parse(&text),
        Err(e) => Err(e.to_string()),
    };
    match v {
        Ok(v)
            if v.get("terms").and_then(Json::as_arr).is_some()
                && v.get("never_translate").and_then(Json::as_strs).is_some() =>
        {
            Vec::new()
        }
        Ok(_) => vec![
            "glossary.json: needs terms, a list, and never_translate, a list of strings".into(),
        ],
        Err(e) => vec![format!("glossary.json: {e}")],
    }
}

/// Budgets, banned patterns and retired terms, for one file of a shared part.
fn policy(rules: &Rules, file: &File, source: bool) -> Vec<String> {
    let mut out = Vec::new();
    for e in file.entries.iter().filter(|e| !rules.debt.contains(&e.key)) {
        for text in e.msg.texts() {
            let mut found = vec![over_budget(rules, &e.key, &text, source)];
            if source {
                found.push(banned(rules, &text));
                found.push(retired(rules, &text));
            }
            out.extend(
                found
                    .into_iter()
                    .flatten()
                    .map(|why| format!("{}: {}: {why}", file.path, e.key)),
            );
        }
    }
    out
}

/// Keys each translation lacks, for every language any part has or ships.
fn missing(cat: &Catalog) -> Vec<String> {
    let mut langs: Vec<&str> = cat
        .parts
        .iter()
        .flat_map(|p| p.translations.iter().map(|f| f.lang.as_str()))
        .collect();
    langs.extend(
        cat.shipped
            .iter()
            .map(String::as_str)
            .filter(|l| *l != cat.source),
    );
    langs.sort();
    langs.dedup();
    let mut out = Vec::new();
    for part in &cat.parts {
        for lang in &langs {
            let tr = part.translations.iter().find(|f| f.lang == *lang);
            for e in part
                .source
                .entries
                .iter()
                .filter(|e| tr.is_none_or(|f| f.get(&e.key).is_none()))
            {
                out.push(format!(
                    "messages/{}/{lang}.json: {}: not translated",
                    part.name, e.key
                ));
            }
        }
    }
    out
}

pub struct Report {
    pub problems: Vec<String>,
    pub missing: Vec<String>,
}

pub fn run(root: &Path) -> Report {
    let (cat, mut problems) = catalog::load(root, true);
    problems.extend(glossary(root));
    problems.extend(shipped_rules(&cat.shipped));
    problems.extend(duplicates(&cat.parts));
    let rules = match Rules::from_json(&cat.rules) {
        Ok(rules) => Some(rules),
        Err(e) => {
            if cat.rules != Json::Null {
                problems.push(format!("rules.json: {e}"));
            }
            None
        }
    };
    if let Some(rules) = &rules {
        problems.extend(stale_debt(rules, &cat.parts));
    }
    for part in &cat.parts {
        for tr in &part.translations {
            problems.extend(orphans(&part.source, tr));
            for e in &tr.entries {
                let Some(src) = part.source.get(&e.key) else {
                    continue;
                };
                problems.extend(
                    placeholders(src, &e.msg).map(|why| format!("{}: {}: {why}", tr.path, e.key)),
                );
            }
        }
        if part.ts_only {
            continue;
        }
        problems.extend(notes(part));
        for file in std::iter::once(&part.source).chain(&part.translations) {
            for e in &file.entries {
                problems.extend(
                    categories(&file.lang, &e.msg)
                        .map(|why| format!("{}: {}: {why}", file.path, e.key)),
                );
            }
            if let Some(rules) = &rules {
                problems.extend(policy(rules, file, file.lang == cat.source));
            }
        }
    }
    Report {
        problems,
        missing: missing(&cat),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{parse_message, parse_pattern, Entry};

    fn rules() -> Rules {
        let text =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/rules.json")).unwrap();
        Rules::from_json(&json::parse(&text).unwrap()).unwrap()
    }

    fn plain(s: &str) -> Message {
        Message::Plain(parse_pattern(s).unwrap())
    }

    fn file(path: &str, lang: &str, entries: &[(&str, &str)]) -> File {
        let entries = entries
            .iter()
            .map(|(k, v)| Entry {
                key: (*k).into(),
                msg: plain(v),
            })
            .collect();
        File {
            lang: lang.into(),
            path: path.into(),
            entries,
        }
    }

    fn part(name: &str, entries: &[(&str, &str)], notes: &[(&str, &str)]) -> Part {
        Part {
            name: name.into(),
            ts_only: false,
            source: file(&format!("messages/{name}/en.json"), "en", entries),
            translations: Vec::new(),
            notes: Some(
                notes
                    .iter()
                    .map(|(k, v)| ((*k).into(), (*v).into()))
                    .collect(),
            ),
        }
    }

    #[test]
    fn budgets_by_role() {
        let r = rules();
        assert_eq!(budget(&r, "player_bitrate_hint", true), 110);
        assert_eq!(budget(&r, "pair_title", true), 60);
        assert_eq!(budget(&r, "notice_saved_body", true), 200);
        assert_eq!(budget(&r, "player_bitrate", true), 40);
        assert_eq!(budget(&r, "player_bitrate", false), 48);
        assert!(over_budget(&r, "player_bitrate", &"x".repeat(40), true).is_none());
        assert_eq!(
            over_budget(&r, "player_bitrate", &"é".repeat(41), true).unwrap(),
            "41 characters, over the budget of 40"
        );
    }

    #[test]
    fn banned_patterns() {
        let r = rules();
        assert!(banned(&r, "Wait here, please.").unwrap().contains("please"));
        assert!(banned(&r, "It worked!").unwrap().contains("exclamation"));
        assert!(banned(&r, "The host failed to wake.").is_some());
        assert!(banned(&r, "The host cannot wake.").is_some());
        assert!(banned(&r, "Couldn't wake the host — it may be asleep.").is_none());
        assert!(banned(&r, "Is it on? Yes!?").is_none());
        assert!(banned(&r, "Displeased hosts").is_none());
    }

    #[test]
    fn retired_terms() {
        let r = rules();
        assert_eq!(
            retired(&r, "Up to 80 Mbps").unwrap(),
            "\"Mbps\" is retired; write \"Mbit/s\""
        );
        assert!(retired(&r, "80 Mb/s").is_some());
        assert!(retired(&r, "Stream to this iPhone.").is_some());
        assert!(retired(&r, "Up to 80 Mbit/s on this device").is_none());
    }

    #[test]
    fn plural_categories() {
        let pl = |arms: &str| {
            let src = format!(
                r#"[{{"declarations": ["local n = count: plural"], "selectors": ["n"], "match": {{{arms}}}}}]"#
            );
            parse_message(&json::parse(&src).unwrap(), false).unwrap()
        };
        assert!(categories("en", &pl(r#""n=one": "a", "n=*": "b""#)).is_none());
        assert_eq!(
            categories("en", &pl(r#""n=*": "b""#)).unwrap(),
            "en plurals need one, other; this has other"
        );
        assert!(categories(
            "pl",
            &pl(r#""n=one": "a", "n=few": "b", "n=many": "c", "n=other": "d""#)
        )
        .is_none());
        assert!(categories("ru", &pl(r#""n=one": "a", "n=other": "d""#)).is_some());
        assert!(categories("ja", &pl(r#""n=other": "d""#)).is_none());
        assert!(categories("en", &plain("x")).is_none());
    }

    #[test]
    fn translation_placeholders() {
        assert!(placeholders(&plain("{a} and {b}"), &plain("{b} und {a}")).is_none());
        assert_eq!(
            placeholders(&plain("{a} x"), &plain("x")).unwrap(),
            "placeholders are none, the source has {a}"
        );
    }

    #[test]
    fn translation_orphans() {
        let src = file("messages/x/en.json", "en", &[("x_a", "A")]);
        let tr = file("messages/x/de.json", "de", &[("x_a", "A"), ("x_b", "B")]);
        assert_eq!(
            orphans(&src, &tr),
            ["messages/x/de.json: x_b: not in messages/x/en.json"]
        );
    }

    #[test]
    fn keys_in_two_parts() {
        let parts = [
            part("a", &[("x_one", "1")], &[]),
            part("b", &[("x_one", "1"), ("x_two", "2")], &[]),
        ];
        assert_eq!(
            duplicates(&parts),
            ["messages/b/en.json: x_one: also in messages/a/en.json"]
        );
    }

    #[test]
    fn notes_cover_placeholders() {
        let p = part(
            "a",
            &[("a_x", "{name} joined"), ("a_y", "Plain")],
            &[("a_gone", "stale")],
        );
        assert_eq!(
            notes(&p),
            [
                "messages/a/notes.json: a_gone: no message has this key",
                "messages/a/notes.json: a_x: has placeholders, so it needs a note"
            ]
        );
    }

    #[test]
    fn shipped_languages_have_rules() {
        assert!(shipped_rules(&["en".into(), "pl".into()]).is_empty());
        assert_eq!(
            shipped_rules(&["ja".into()]),
            ["locales.json: ja ships without a plural rule in src/plural.rs"]
        );
    }

    #[test]
    fn debt_names_real_keys() {
        let mut r = rules();
        r.debt = vec!["a_x".into(), "a_gone".into()];
        assert_eq!(
            stale_debt(&r, &[part("a", &[("a_x", "x")], &[])]),
            ["rules.json: debt: a_gone: no part has this key"]
        );
    }

    #[test]
    fn policy_skips_debt_and_spares_translations() {
        let mut r = rules();
        let long = "y".repeat(45);
        let f = file(
            "messages/a/en.json",
            "en",
            &[("a_x", &long), ("a_y", "Done!")],
        );
        assert_eq!(policy(&r, &f, true).len(), 2);
        assert!(policy(
            &r,
            &file(
                "messages/a/de.json",
                "de",
                &[("a_x", &long), ("a_y", "Fertig!")]
            ),
            false
        )
        .is_empty());
        r.debt = vec!["a_x".into(), "a_y".into()];
        assert!(policy(&r, &f, true).is_empty());
    }

    #[test]
    fn seed_is_clean() {
        let report = run(Path::new(env!("CARGO_MANIFEST_DIR")));
        assert!(report.problems.is_empty(), "{}", report.problems.join("\n"));
    }

    #[test]
    fn bad_fixture_matches_golden() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture-bad");
        let report = run(&root);
        let missing = report.missing.iter().map(|m| format!("missing: {m}"));
        let got: Vec<String> = report.problems.iter().cloned().chain(missing).collect();
        crate::golden("check-bad.txt", &(got.join("\n") + "\n"));
    }
}
