//! Punktfunk's messages, compiled in. One module per part of `messages/` (`common` always, the
//! rest behind a Cargo feature of the same name) and one function per message, named as its key:
//!
//! ```
//! pf_l10n::set_languages(&["de-AT", "en"]);
//! let label: &'static str = pf_l10n::common::common_cancel();
//! ```
//!
//! A message without placeholders returns `&'static str`; one with placeholders takes them in
//! the order the English text shows them and returns `String`; a plural's count is a `u64`.
//! [`get`] finds a message by key where the key is data.
//!
//! The language is process-wide and English until [`set_languages`] picks a shipped one. A
//! message missing in it falls back to English. [`with_language`] overrides it on one thread,
//! for tests. Every shipped language is static data; nothing is read at run time. The format is
//! `SPEC.md`; how to add a message is `README.md`.

use std::borrow::Cow;
use std::cell::Cell;
use std::fmt::{Display, Write as _};
use std::sync::atomic::{AtomicUsize, Ordering};

pub mod plural;
use plural::Category;

include!(concat!(env!("OUT_DIR"), "/messages.rs"));

#[cfg(test)]
#[allow(dead_code)]
mod catalog;
#[cfg(test)]
#[allow(dead_code)]
mod json;
#[cfg(test)]
mod tests;

// Only the generated tables build these, and only generated functions call `text` and
// `format`. A build whose parts hold no plural or placeholder leaves some unused.
#[allow(dead_code)]
#[derive(Debug)]
enum Seg {
    Text(&'static str),
    Arg(usize),
}

#[allow(dead_code)]
#[derive(Debug)]
enum Body {
    Plain(&'static [Seg]),
    Plural(&'static [(Category, &'static [Seg])]),
}

/// One message in every compiled language: `langs[i]` is `LANGS[i]`'s text, `None` when that
/// language lacks it.
#[derive(Debug)]
struct Message {
    key: &'static str,
    /// Placeholder names in English order: the arguments of the generated function.
    args: &'static [&'static str],
    /// The index in `args` of a plural's count.
    count: Option<usize>,
    langs: &'static [Option<Body>],
}

static LANGUAGE: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    // Already `const`; clippy flags it anyway on an Android target.
    #[allow(clippy::missing_const_for_thread_local)]
    static OVERRIDE: Cell<Option<usize>> = const { Cell::new(None) };
}

fn current() -> usize {
    OVERRIDE
        .with(Cell::get)
        .unwrap_or_else(|| LANGUAGE.load(Ordering::Relaxed))
}

/// Picks the best shipped language for the OS's preferred list, most preferred first, and
/// returns it. Each tag is tried exactly, through `locales.json`'s fallback, then one subtag
/// shorter (`de-AT`, then `de`); English when none matches.
pub fn set_languages<S: AsRef<str>>(tags: &[S]) -> &'static str {
    let i = pick(tags, LANGS, FALLBACK);
    LANGUAGE.store(i, Ordering::Relaxed);
    LANGS[i]
}

/// The language messages are read in on this thread.
pub fn language() -> &'static str {
    LANGS[current()]
}

/// Runs `f` with this thread reading `tag` (picked as [`set_languages`] picks), leaving every
/// other thread and the process-wide language alone.
pub fn with_language<R>(tag: &str, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<usize>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OVERRIDE.with(|o| o.set(self.0));
        }
    }
    let lang = pick(&[tag], LANGS, FALLBACK);
    let _restore = Restore(OVERRIDE.with(|o| o.replace(Some(lang))));
    f()
}

/// The index in `langs` of the best match for `tags`; 0, the source, when none matches.
fn pick<S: AsRef<str>>(tags: &[S], langs: &[&str], fallback: &[(&str, &str)]) -> usize {
    let find = |t: &str| langs.iter().position(|l| l.eq_ignore_ascii_case(t));
    for tag in tags {
        let mut t = tag.as_ref().trim().replace('_', "-");
        loop {
            let via = fallback
                .iter()
                .find(|(from, _)| from.eq_ignore_ascii_case(&t))
                .and_then(|&(_, to)| find(to));
            if let Some(i) = find(&t).or(via) {
                return i;
            }
            match t.rfind('-') {
                Some(cut) => t.truncate(cut),
                None => break,
            }
        }
    }
    0
}

/// The segments `m` reads in `lang` for `count`: that language's text, else English. A plural
/// without the count's category reads `other`.
fn variant(m: &Message, langs: &[&str], lang: usize, count: Option<u64>) -> &'static [Seg] {
    let bodies = m.langs;
    let read = |l: usize| -> Option<&'static [Seg]> {
        match bodies.get(l)?.as_ref()? {
            Body::Plain(segs) => Some(*segs),
            Body::Plural(cases) => {
                let want = count.map_or(Category::Other, |n| plural::category(langs[l], n));
                let find = |c: Category| cases.iter().find(|(k, _)| *k == c).map(|(_, s)| *s);
                find(want).or_else(|| find(Category::Other))
            }
        }
    };
    read(lang).or_else(|| read(0)).unwrap_or(&[])
}

/// Writes `segs`, a placeholder `arg` has no value for as `{name}`.
fn render<'a>(
    segs: &[Seg],
    names: &[&str],
    arg: impl Fn(usize) -> Option<&'a dyn Display>,
) -> String {
    let mut out = String::new();
    for seg in segs {
        match *seg {
            Seg::Text(t) => out.push_str(t),
            Seg::Arg(i) => {
                let _ = match arg(i) {
                    Some(v) => write!(out, "{v}"),
                    None => write!(out, "{{{}}}", names[i]),
                };
            }
        }
    }
    out
}

#[allow(dead_code)]
fn text(m: &Message) -> &'static str {
    match variant(m, LANGS, current(), None) {
        [Seg::Text(t)] => t,
        _ => "",
    }
}

#[allow(dead_code)]
fn format(m: &Message, count: Option<u64>, args: &[&dyn Display]) -> String {
    let segs = variant(m, LANGS, current(), count);
    render(segs, m.args, |i| args.get(i).copied())
}

/// A message found by key. It is a reference into static data: copy it freely.
#[derive(Clone, Copy, Debug)]
pub struct Msg(&'static Message);

/// The message `key` from the parts this build compiles in, for where a key is data: a setting
/// id, a host error code.
pub fn get(key: &str) -> Option<Msg> {
    PARTS.iter().find_map(|all| {
        let i = all.binary_search_by(|m| m.key.cmp(key)).ok()?;
        Some(Msg(all[i]))
    })
}

impl Msg {
    pub fn key(self) -> &'static str {
        self.0.key
    }

    /// The text without arguments: borrowed for a message that has none. A placeholder shows
    /// as `{name}`; a plural reads `other`.
    pub fn text(self) -> Cow<'static, str> {
        match variant(self.0, LANGS, current(), None) {
            [Seg::Text(t)] => Cow::Borrowed(*t),
            segs => Cow::Owned(render(segs, self.0.args, |_| None)),
        }
    }

    /// Fills placeholders by name. One without a value shows as `{name}`.
    pub fn format(self, args: &[(&str, &dyn Display)]) -> String {
        self.fill(None, args)
    }

    /// A plural message for `count`. The count also fills its own placeholder.
    pub fn format_count(self, count: u64, args: &[(&str, &dyn Display)]) -> String {
        self.fill(Some(count), args)
    }

    fn fill(self, count: Option<u64>, args: &[(&str, &dyn Display)]) -> String {
        let m = self.0;
        let n = count.unwrap_or_default();
        let segs = variant(m, LANGS, current(), count);
        render(segs, m.args, |i| {
            let named = args.iter().find(|(name, _)| *name == m.args[i]);
            match named {
                Some((_, v)) => Some(*v),
                None if count.is_some() && m.count == Some(i) => Some(&n as &dyn Display),
                None => None,
            }
        })
    }
}

/// The OS's preferred UI languages, most preferred first, for [`set_languages`].
#[cfg(all(feature = "os", any(windows, target_os = "macos")))]
pub fn os_languages() -> Vec<String> {
    sys_locale::get_locales().collect()
}

/// The OS's preferred UI languages, most preferred first, for [`set_languages`].
#[cfg(all(feature = "os", not(any(windows, target_os = "macos"))))]
pub fn os_languages() -> Vec<String> {
    posix_languages(|name| std::env::var(name).ok())
}

/// gettext's order: `LANGUAGE`'s list unless the locale is `C`, then the first of `LC_ALL`,
/// `LC_MESSAGES` and `LANG` that is set. `de_AT.UTF-8@euro` becomes `de-AT`.
#[cfg(any(test, all(feature = "os", not(any(windows, target_os = "macos")))))]
fn posix_languages(var: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let tag = |s: &str| {
        let t = s.split(['.', '@']).next().unwrap_or("").replace('_', "-");
        (!t.is_empty() && t != "C" && t != "POSIX").then_some(t)
    };
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(&var)
        .find(|v| !v.is_empty());
    let Some(locale) = locale.as_deref().and_then(tag) else {
        return Vec::new();
    };
    let mut out: Vec<String> = var("LANGUAGE")
        .unwrap_or_default()
        .split(':')
        .filter_map(tag)
        .collect();
    out.push(locale);
    out
}
