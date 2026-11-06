use std::fmt::Display;

use crate::catalog::{self, Compiled, Ix};
use crate::json::{self, Json};
use crate::{pick, posix_languages, render, variant, Body, Message, Seg};

fn leak<T>(v: Vec<T>) -> &'static [T] {
    Box::leak(v.into_boxed_slice())
}

fn leak_str(s: &str) -> &'static str {
    Box::leak(s.to_owned().into_boxed_str())
}

fn segs(ix: &[Ix]) -> &'static [Seg] {
    leak(
        ix.iter()
            .map(|i| match i {
                Ix::Text(t) => Seg::Text(leak_str(t)),
                Ix::Arg(n) => Seg::Arg(*n),
            })
            .collect(),
    )
}

fn body(c: Compiled) -> Body {
    match c {
        Compiled::Plain(ix) => Body::Plain(segs(&ix)),
        Compiled::Plural(cases) => {
            Body::Plural(leak(cases.iter().map(|(k, ix)| (*k, segs(ix))).collect()))
        }
    }
}

fn s<'a>(v: &'a Json, key: &str) -> &'a str {
    v.get(key)
        .and_then(Json::as_str)
        .unwrap_or_else(|| panic!("{key} in {v:?}"))
}

#[test]
fn vectors() {
    let v = json::parse(include_str!("../vectors.json")).expect("vectors.json");
    let msgs = v.get("messages").and_then(Json::as_obj).expect("messages");
    let langs: Vec<&str> = msgs.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(langs[0], "en", "English comes first");
    let parse = |l: usize, key: &str| {
        let m = msgs[l].1.get(key)?;
        Some(
            catalog::parse_message(m, false).unwrap_or_else(|e| panic!("{}: {key}: {e}", langs[l])),
        )
    };
    for case in v.get("format").and_then(Json::as_arr).expect("format") {
        let (key, want) = (s(case, "key"), s(case, "want"));
        let lang = langs
            .iter()
            .position(|l| *l == s(case, "lang"))
            .expect("a language of messages");
        let en = parse(0, key).expect("an English message");
        let args = en.args();
        let bodies: Vec<Option<Body>> = (0..langs.len())
            .map(|l| parse(l, key).map(|m| body(catalog::compile(&en, &m).expect("compiles"))))
            .collect();
        let m = Message {
            key: leak_str(key),
            args: leak(args.iter().map(|a| leak_str(a)).collect()),
            count: en.count().and_then(|c| args.iter().position(|a| a == c)),
            langs: leak(bodies),
        };
        let given = case.get("args").expect("args");
        let count = en
            .count()
            .and_then(|c| given.get(c)?.as_f64())
            .map(|n| n as u64);
        let values: Vec<Option<String>> = args
            .iter()
            .map(|a| match given.get(a)? {
                Json::Str(s) => Some(s.clone()),
                Json::Num(n) => Some((*n as u64).to_string()),
                other => panic!("argument {a}: {other:?}"),
            })
            .collect();
        let got = render(variant(&m, &langs, lang, count), m.args, |i| {
            values[i].as_ref().map(|v| v as &dyn Display)
        });
        assert_eq!(got, want, "{key} in {}", langs[lang]);
    }
    for case in v.get("pick").and_then(Json::as_arr).expect("pick") {
        let strs = |k: &str| case.get(k).and_then(Json::as_strs).expect("a list");
        let (os, shipped) = (strs("os"), strs("shipped"));
        let shipped: Vec<&str> = shipped.iter().map(String::as_str).collect();
        let fallback: Vec<(&str, &str)> = case
            .get("fallback")
            .and_then(Json::as_obj)
            .expect("fallback")
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str().expect("a tag")))
            .collect();
        assert_eq!(
            shipped[pick(&os, &shipped, &fallback)],
            s(case, "want"),
            "{os:?}"
        );
    }
}

#[test]
fn seed_common() {
    assert_eq!(crate::common::common_cancel(), "Cancel");
    assert_eq!(
        crate::get("common_cancel").map(|m| m.text()),
        Some("Cancel".into())
    );
    assert!(crate::get("common_nothing_here").is_none());
}

#[cfg(feature = "client")]
#[test]
fn seed_client() {
    use crate::client::*;
    assert_eq!(player_stats_scale_pct(), "Statistics size");
    assert_eq!(prefs_advanced_changed(1), "1 advanced setting changed");
    assert_eq!(prefs_advanced_changed(3), "3 advanced settings changed");
    let m = crate::get("prefs_advanced_changed").expect("client is compiled in");
    assert_eq!(m.format_count(1, &[]), "1 advanced setting changed");
    assert_eq!(
        m.format_count(2, &[("count", &"two")]),
        "two advanced settings changed"
    );
    assert_eq!(m.text(), "{count} advanced settings changed");
}

#[test]
fn get_searches_only_compiled_parts() {
    assert_eq!(
        crate::get("player_stats_scale_pct").is_some(),
        cfg!(feature = "client")
    );
}

#[test]
fn language_override_stays_on_its_thread() {
    assert_eq!(crate::set_languages(&["ja-JP"]), "en");
    crate::with_language("de-AT", || assert_eq!(crate::language(), "en"));
    #[cfg(feature = "pseudo")]
    crate::with_language("en-XA", || {
        assert_eq!(crate::language(), "en-XA");
        assert_eq!(crate::common::common_cancel(), "[Çáñçéĺ ~~]");
        let other = std::thread::spawn(|| (crate::language(), crate::common::common_cancel()));
        assert_eq!(other.join().unwrap(), ("en", "Cancel"));
    });
    assert_eq!(crate::language(), "en");
}

#[cfg(all(feature = "pseudo", feature = "client"))]
#[test]
fn pseudo_keeps_placeholders() {
    crate::with_language("en-XA", || {
        let text = crate::client::prefs_advanced_changed(3);
        assert_eq!(text, "[3 áďváñçéď šéťťíñğš çĥáñğéď ~~~~~~~~~~~]");
    });
}

fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |name: &str| {
        pairs
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.to_string())
    }
}

#[test]
fn posix_order() {
    let got = posix_languages(env(&[("LANGUAGE", "de_AT:fr"), ("LANG", "de_AT.UTF-8")]));
    assert_eq!(got, ["de-AT", "fr", "de-AT"]);
    let c = posix_languages(env(&[
        ("LANGUAGE", "fr"),
        ("LC_ALL", "C.UTF-8"),
        ("LANG", "de_DE.UTF-8"),
    ]));
    assert!(c.is_empty());
    assert_eq!(
        posix_languages(env(&[("LC_MESSAGES", "sr_RS@latin")])),
        ["sr-RS"]
    );
    assert!(posix_languages(env(&[])).is_empty());
}

#[cfg(feature = "os")]
#[test]
fn os_languages_answers() {
    let _ = crate::os_languages();
}
