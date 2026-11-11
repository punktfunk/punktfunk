//! The form a library search compares. The query and every title fold alike, then the title
//! has to contain the query; host and console both search this way.
//!
//! The fold lowercases, then applies the parts of NFKC a CJK keyboard reaches: full-width
//! ASCII and space, half-width katakana, a kana and a combining voiced mark, conjoining
//! Hangul jamo. Katakana then reads as hiragana, so ゼルダ, ぜるだ and ｾﾞﾙﾀﾞ are one
//! word; the prolonged sound mark ー stays. Nothing is stripped: `é` does not match `e`.
//!
//! Not the rest of NFKC: decomposed Latin, ligatures, circled or Roman-numeral characters,
//! compatibility jamo and the spacing voiced marks ゛゜ stay as typed.

/// `text` in the form [`str::contains`] compares for a search.
pub fn fold(text: &str) -> String {
    let lower = text.to_lowercase();
    if lower.is_ascii() {
        return lower;
    }
    let mut out = String::with_capacity(lower.len());
    // Held back one char: the next may be a mark or a jamo that composes with it.
    let mut held: Option<char> = None;
    for c in lower.chars().map(compat) {
        match held.and_then(|h| compose(h, c)) {
            Some(both) => held = Some(both),
            None => {
                out.extend(held.map(hiragana));
                held = Some(c);
            }
        }
    }
    out.extend(held.map(hiragana));
    out
}

/// U+FF61–FF9F as NFKC maps them. The two marks at the end are the combining voiced marks.
const HALF_WIDTH_KANA: [char; 63] = [
    '。', '「', '」', '、', '・', 'ヲ', 'ァ', 'ィ', 'ゥ', 'ェ', 'ォ', 'ャ', 'ュ', 'ョ', 'ッ', 'ー',
    'ア', 'イ', 'ウ', 'エ', 'オ', 'カ', 'キ', 'ク', 'ケ', 'コ', 'サ', 'シ', 'ス', 'セ', 'ソ', 'タ',
    'チ', 'ツ', 'テ', 'ト', 'ナ', 'ニ', 'ヌ', 'ネ', 'ノ', 'ハ', 'ヒ', 'フ', 'ヘ', 'ホ', 'マ', 'ミ',
    'ム', 'メ', 'モ', 'ヤ', 'ユ', 'ヨ', 'ラ', 'リ', 'ル', 'レ', 'ロ', 'ワ', 'ン', '\u{3099}',
    '\u{309A}',
];

/// Full-width ASCII and the ideographic space as ASCII, half-width kana at full width.
fn compat(c: char) -> char {
    match c as u32 {
        0x3000 => ' ',
        n @ 0xFF01..=0xFF5E => char::from_u32(n - 0xFEE0).unwrap_or(c),
        n @ 0xFF61..=0xFF9F => HALF_WIDTH_KANA[(n - 0xFF61) as usize],
        _ => c,
    }
}

/// The precomposed character canonical composition makes of `a` then `b`, if any.
fn compose(a: char, b: char) -> Option<char> {
    let (a, b) = (a as u32, b as u32);
    let to = match b {
        // Hangul: a leading consonant and a vowel, then that syllable and a final.
        0x1161..=0x1175 if (0x1100..=0x1112).contains(&a) => {
            0xAC00 + ((a - 0x1100) * 21 + (b - 0x1161)) * 28
        }
        0x11A8..=0x11C2 if (0xAC00..=0xD7A3).contains(&a) && (a - 0xAC00) % 28 == 0 => {
            a + (b - 0x11A7)
        }
        0x3099 => voiced(a)?,
        0x309A if ha_row(kana_row(a)) => a + 2,
        _ => return None,
    };
    char::from_u32(to)
}

/// Katakana sits 0x60 above hiragana, row for row; compare kana in hiragana.
fn kana_row(a: u32) -> u32 {
    if (0x30A1..=0x30FE).contains(&a) {
        a - 0x60
    } else {
        a
    }
}

/// は ひ ふ へ ほ: the one row that takes both marks.
fn ha_row(h: u32) -> bool {
    matches!(h, 0x306F | 0x3072 | 0x3075 | 0x3078 | 0x307B)
}

/// The voiced form of kana `a`, which is the next code point except for う and ワ ヰ ヱ ヲ.
fn voiced(a: u32) -> Option<u32> {
    match kana_row(a) {
        h @ 0x304B..=0x3061 if h % 2 == 1 => Some(a + 1),
        0x3064 | 0x3066 | 0x3068 | 0x309D => Some(a + 1),
        h if ha_row(h) => Some(a + 1),
        0x3046 => Some(a + 0x4E),
        0x308F..=0x3092 if a > 0x30A0 => Some(a + 8),
        _ => None,
    }
}

/// U+30A1–30F6 as the hiragana U+3041–3096. ー and the katakana-only letters stay.
fn hiragana(c: char) -> char {
    match c as u32 {
        n @ 0x30A1..=0x30F6 => char::from_u32(n - 0x60).unwrap_or(c),
        _ => c,
    }
}

#[cfg(test)]
mod tests {
    use super::fold;

    fn finds(title: &str, query: &str) -> bool {
        fold(title).contains(&fold(query))
    }

    #[test]
    fn kana_of_either_script_and_width_match() {
        for q in ["ゼルダ", "ぜるだ", "ｾﾞﾙﾀﾞ", "セ\u{3099}ルタ\u{3099}"] {
            assert!(finds("ゼルダの伝説", q), "{q}");
        }
        assert_eq!(fold("ｾﾞﾙﾀﾞ"), "ぜるだ");
        assert_eq!(fold("か\u{3099}"), "が");
        assert_eq!(fold("ﾊﾟｯｸﾏﾝ"), "ぱっくまん");
        // The prolonged sound mark is no vowel; it stays.
        assert_eq!(fold("ファイナルファンタジー"), "ふぁいなるふぁんたじー");
        assert_eq!(fold("ｰ"), "ー");
        // Composed before the shift: ヷ and ヾ have no hiragana twin and stay.
        assert_eq!(fold("ワ\u{3099}"), "ヷ");
        assert_eq!(fold("ウ\u{3099}"), "ゔ");
        assert_eq!(fold("ヽ\u{3099}"), "ヾ");
        // A mark with nothing to sit on stays a mark.
        assert_eq!(fold("ﾞ"), "\u{3099}");
        assert_eq!(fold("ん\u{3099}"), "ん\u{3099}");
    }

    #[test]
    fn full_width_latin_digits_and_space_are_ascii() {
        assert_eq!(fold("ＦＩＮＡＬ"), "final");
        assert!(finds("Final Fantasy VII", "ＦＩＮＡＬ"));
        assert!(finds("Persona 5 Royal", "５"));
        assert!(finds("ペルソナ7", "７"));
        assert!(finds("ドラゴンクエスト　XI", "ドラゴンクエスト xi"));
    }

    #[test]
    fn hangul_jamo_compose_into_syllables() {
        assert_eq!(fold("\u{1100}\u{1161}"), "가");
        assert_eq!(fold("\u{1112}\u{1161}\u{11AB}"), "한");
        assert!(finds(
            "배틀그라운드",
            "\u{1107}\u{1162}\u{1110}\u{1173}\u{11AF}"
        ));
        // A final on a syllable that already has one does not compose.
        assert_eq!(fold("한\u{11A8}"), "한\u{11A8}");
    }

    #[test]
    fn chinese_ascii_and_accents_are_as_before() {
        for t in [
            "原神",
            "仙剑奇侠传",
            "Pokémon",
            "Hello Wörld",
            "ΟΔΥΣΣΕΑΣ",
            "Straße",
        ] {
            assert_eq!(fold(t), t.to_lowercase(), "{t}");
        }
        assert!(finds("Pokémon Violet", "POKÉ"));
        assert!(!finds("Pokémon Violet", "pokemon"));
        assert!(!finds("Pokemon Violet", "poké"));
    }
}
