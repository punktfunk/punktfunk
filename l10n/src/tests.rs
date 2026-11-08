use std::fmt::Display;

use crate::catalog::{self, Compiled, Ix};
use crate::json::{self, Json};
use crate::plural::Category;
use crate::{pick, posix_languages, render, variant, Message, Pack};

fn leak<T>(v: Vec<T>) -> &'static [T] {
    Box::leak(v.into_boxed_slice())
}

fn leak_str(s: &str) -> &'static str {
    Box::leak(s.to_owned().into_boxed_str())
}

/// A language holding one message, at index 0, or none.
fn pack(lang: &str, body: Option<Compiled>) -> Pack {
    let text = body.map_or(String::new(), |c| c.pack().expect("packs"));
    Pack {
        lang: leak_str(lang),
        ends: leak(vec![text.len() as u32]),
        text: leak_str(&text),
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
        let packs: Vec<Pack> = (0..langs.len())
            .map(|l| {
                let body = parse(l, key).map(|m| catalog::compile(&en, &m).expect("compiles"));
                pack(langs[l], body)
            })
            .collect();
        let m = Message {
            key: leak_str(key),
            args: leak(args.iter().map(|a| leak_str(a)).collect()),
            count: en.count().and_then(|c| args.iter().position(|a| a == c)),
            ix: 0,
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
        let got = render(variant(&m, &packs, lang, count), m.args, |i| {
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
fn bodies_sit_side_by_side_in_a_pack() {
    let t = |s: &str| Ix::Text(s.into());
    let plain = Compiled::Plain(vec![t("a "), Ix::Arg(1), t(" b")]);
    let plural = Compiled::Plural(vec![
        (Category::One, vec![Ix::Arg(0), t(" one")]),
        (Category::Other, vec![t("many")]),
    ]);
    let (a, p) = (plain.pack().unwrap(), plural.pack().unwrap());
    let ends = vec![a.len() as u32, a.len() as u32, (a.len() + p.len()) as u32];
    let pk = Pack {
        lang: "en",
        text: leak_str(&format!("{a}{p}")),
        ends: leak(ends),
    };
    assert_eq!((pk.body(0), pk.body(1), pk.body(2)), (&*a, "", &*p));
    for bad in ["x\u{1}", "\u{2}x"] {
        assert!(Compiled::Plain(vec![t(bad)]).pack().is_err(), "{bad:?}");
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
    crate::with_language("de-AT", || assert_eq!(crate::language(), "de"));
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
