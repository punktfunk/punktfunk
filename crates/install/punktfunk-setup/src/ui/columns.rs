//! Text measured in terminal columns, not chars.
//!
//! A CJK character takes two columns, and CJK prose has no spaces to break at. Every width
//! the screens compute or pad to goes through here, so the answer is the same for all of them.

use std::borrow::Cow;

use console::{measure_text_width, pad_str, Alignment};

/// Columns one char takes: 0 for a combining mark, 2 for a CJK ideograph.
pub fn of_char(c: char) -> usize {
    measure_text_width(c.encode_utf8(&mut [0; 4]))
}

/// `text` padded with spaces on the right to `width` columns. Wider text is left whole.
pub fn pad(text: &str, width: usize) -> Cow<'_, str> {
    pad_str(text, width, Alignment::Left, None)
}

/// Greedy word wrap to `width` columns. A word wider than a whole line, such as spaceless CJK
/// prose or a long path, is broken at the column limit, never inside a character.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut used = 0;
    for word in text.split_whitespace() {
        let cols = measure_text_width(word);
        let gap = usize::from(used > 0);
        if used + gap + cols <= width {
            if gap == 1 {
                cur.push(' ');
            }
            cur.push_str(word);
            used += gap + cols;
        } else if cols <= width {
            lines.push(std::mem::replace(&mut cur, word.to_string()));
            used = cols;
        } else {
            if gap == 1 {
                cur.push(' ');
                used += 1;
            }
            for c in word.chars() {
                let cols = of_char(c);
                if used > 0 && used + cols > width {
                    lines.push(cur.trim_end().to_string());
                    cur.clear();
                    used = 0;
                }
                cur.push(c);
                used += cols;
            }
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ascii_label_pads_exactly_as_format_does() {
        for text in ["", "Channel", "Start at boot", "Gerät"] {
            for width in [0, 3, 7, 16] {
                assert_eq!(
                    pad(text, width),
                    format!("{text:<width$}"),
                    "{text:?}/{width}"
                );
            }
        }
    }

    #[test]
    fn a_japanese_label_pads_to_the_column_an_ascii_one_does() {
        let (ascii, japanese) = (pad("Channel", 12), pad("チャンネル", 12));
        assert_eq!(measure_text_width(&ascii), 12);
        assert_eq!(measure_text_width(&japanese), 12);
        assert_eq!(japanese, "チャンネル  ");
    }

    #[test]
    fn a_char_takes_the_columns_the_terminal_gives_it() {
        assert_eq!(of_char('a'), 1);
        assert_eq!(of_char('ä'), 1);
        assert_eq!(of_char('田'), 2);
        assert_eq!(of_char('\u{0301}'), 0);
    }

    #[test]
    fn spaced_prose_wraps_between_words() {
        assert_eq!(
            wrap("the quick brown fox jumps over the lazy dog", 20),
            ["the quick brown fox", "jumps over the lazy", "dog"]
        );
    }

    #[test]
    fn a_spaceless_cjk_paragraph_wraps_inside_the_limit() {
        let text = "接続を開始するにはEnterキーを押してください。ホストのインストールが終わると、サービスが自動的に起動します。";
        let lines = wrap(text, 24);
        assert!(lines.len() > 3, "{lines:?}");
        for line in &lines {
            assert!(measure_text_width(line) <= 24, "{line:?}");
            assert!(!line.is_empty());
        }
        assert_eq!(lines.concat(), text, "nothing lost, nothing split");
    }

    #[test]
    fn a_wide_char_that_does_not_fit_starts_the_next_line() {
        assert_eq!(wrap("田中田中田", 5), ["田中", "田中", "田"]);
    }

    #[test]
    fn a_long_path_breaks_at_the_limit_after_the_words_before_it() {
        let lines = wrap(r"Folder C:\Users\田中\AppData\Local\Punktfunk", 20);
        assert_eq!(lines[0], r"Folder C:\Users\田中");
        assert_eq!(
            lines.concat().replace(' ', ""),
            r"FolderC:\Users\田中\AppData\Local\Punktfunk"
        );
        for line in &lines {
            assert!(measure_text_width(line) <= 20, "{line:?}");
        }
    }
}
