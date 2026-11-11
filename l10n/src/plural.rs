//! CLDR cardinal plural rules, for integer counts.
//!
//! [`categories`] is the set a language's plural message must cover: what `Intl.PluralRules`
//! and Android report, so Paraglide and `<plurals>` agree with this crate. [`category`] picks
//! one for a count. A language without a rule here is read as `other` and cannot ship. The tests
//! pin each rule to CLDR's examples.

/// A CLDR plural category, in CLDR's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    Zero,
    One,
    Two,
    Few,
    Many,
    Other,
}

impl Category {
    pub const ALL: [Category; 6] = [
        Category::Zero,
        Category::One,
        Category::Two,
        Category::Few,
        Category::Many,
        Category::Other,
    ];

    /// The CLDR name, as message files and Android write it.
    pub fn name(self) -> &'static str {
        match self {
            Category::Zero => "zero",
            Category::One => "one",
            Category::Two => "two",
            Category::Few => "few",
            Category::Many => "many",
            Category::Other => "other",
        }
    }

    pub fn from_name(name: &str) -> Option<Category> {
        Category::ALL.into_iter().find(|c| c.name() == name)
    }
}

#[derive(Clone, Copy)]
enum Rule {
    /// Japanese, Korean, Chinese and the like: one form for every count.
    Other,
    One,
    OneMillion,
    ZeroOneMillion,
    Polish,
    Czech,
    EastSlavic,
}

fn rule(lang: &str) -> Option<Rule> {
    let mut subtags = lang.split(['-', '_']);
    let primary = subtags.next().unwrap_or("").to_ascii_lowercase();
    let region = subtags.next().unwrap_or("");
    Some(match primary.as_str() {
        "ja" | "ko" | "zh" | "vi" | "th" | "id" | "ms" => Rule::Other,
        "en" | "de" | "nl" | "sv" | "da" | "nb" | "fi" | "tr" => Rule::One,
        "pt" if region.eq_ignore_ascii_case("pt") => Rule::OneMillion,
        "es" | "it" => Rule::OneMillion,
        "fr" | "pt" => Rule::ZeroOneMillion,
        "pl" => Rule::Polish,
        "cs" => Rule::Czech,
        "ru" | "uk" => Rule::EastSlavic,
        _ => return None,
    })
}

/// The categories a plural message in `lang` must have, or `None` when no rule is known.
pub fn categories(lang: &str) -> Option<&'static [Category]> {
    use Category::*;
    Some(match rule(lang)? {
        Rule::Other => &[Other],
        Rule::One => &[One, Other],
        Rule::OneMillion | Rule::ZeroOneMillion => &[One, Many, Other],
        Rule::Polish | Rule::Czech | Rule::EastSlavic => &[One, Few, Many, Other],
    })
}

/// The category of `n` in `lang`. `other` for a language without a rule.
pub fn category(lang: &str, n: u64) -> Category {
    let few = matches!(n % 10, 2..=4) && !matches!(n % 100, 12..=14);
    let million = n != 0 && n % 1_000_000 == 0;
    let Some(rule) = rule(lang) else {
        return Category::Other;
    };
    match rule {
        Rule::One if n == 1 => Category::One,
        Rule::OneMillion if n == 1 => Category::One,
        Rule::ZeroOneMillion if n <= 1 => Category::One,
        Rule::OneMillion | Rule::ZeroOneMillion if million => Category::Many,
        Rule::Polish if n == 1 => Category::One,
        Rule::Polish if few => Category::Few,
        Rule::Polish => Category::Many,
        // `many` is Czech for fractions, which a count never is.
        Rule::Czech => match n {
            1 => Category::One,
            2..=4 => Category::Few,
            _ => Category::Other,
        },
        Rule::EastSlavic if n % 10 == 1 && n % 100 != 11 => Category::One,
        Rule::EastSlavic if few => Category::Few,
        Rule::EastSlavic => Category::Many,
        _ => Category::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::Category::{self, *};
    use super::{categories, category};

    const COUNTS: [u64; 12] = [0, 1, 2, 5, 11, 21, 22, 25, 100, 101, 102, 1_000_000];

    fn expect(lang: &str, want: [Category; 12]) {
        let got: Vec<Category> = COUNTS.iter().map(|&n| category(lang, n)).collect();
        assert_eq!(got, want, "{lang} for {COUNTS:?}");
        let set = categories(lang).expect("a rule");
        assert!(
            got.iter().all(|c| set.contains(c)),
            "{lang} picks outside {set:?}"
        );
    }

    #[test]
    fn one_other() {
        for lang in [
            "en", "de", "nl", "sv", "da", "nb", "fi", "tr", "en-US", "de_AT",
        ] {
            let o = Other;
            expect(lang, [o, One, o, o, o, o, o, o, o, o, o, o]);
            assert_eq!(categories(lang), Some(&[One, Other][..]));
        }
    }

    #[test]
    fn one_many_other() {
        let o = Other;
        for lang in ["es", "it", "pt-PT"] {
            expect(lang, [o, One, o, o, o, o, o, o, o, o, o, Many]);
        }
        for lang in ["fr", "pt", "pt-BR"] {
            expect(lang, [One, One, o, o, o, o, o, o, o, o, o, Many]);
        }
    }

    #[test]
    fn polish() {
        let m = Many;
        expect("pl", [m, One, Few, m, m, m, Few, m, m, m, Few, m]);
    }

    #[test]
    fn czech() {
        let o = Other;
        expect("cs", [o, One, Few, o, o, o, o, o, o, o, o, o]);
        assert_eq!(categories("cs"), Some(&[One, Few, Many, Other][..]));
    }

    #[test]
    fn russian_ukrainian() {
        let m = Many;
        for lang in ["ru", "uk"] {
            expect(lang, [m, One, Few, m, m, One, Few, m, m, One, Few, m]);
        }
    }

    #[test]
    fn other_only() {
        for lang in [
            "ja", "ko", "zh", "vi", "th", "id", "ms", "ja-JP", "zh-Hant", "zh_Hans",
        ] {
            expect(lang, [Other; 12]);
            assert_eq!(categories(lang), Some(&[Other][..]), "{lang}");
        }
    }

    #[test]
    fn unknown_language_is_other() {
        assert_eq!(categories("ar"), None);
        assert_eq!(category("ar", 1), Other);
    }

    #[test]
    fn names_round_trip() {
        for c in Category::ALL {
            assert_eq!(Category::from_name(c.name()), Some(c));
        }
        assert_eq!(Category::from_name("*"), None);
    }
}
