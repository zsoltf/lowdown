pub fn first_sentence_or_line(text: &str) -> String {
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((head, _)) = split_sentence(line) {
            return head.to_string();
        }
        return line.to_string();
    }
    String::new()
}

pub fn scanline_summary(text: &str, max_words: usize, max_chars: usize) -> String {
    let first = first_sentence_or_line(text);
    let compact = first
        .split_whitespace()
        .take(max_words)
        .collect::<Vec<_>>()
        .join(" ");
    crop_chars(&compact, max_chars)
}

pub fn crop_chars(text: &str, max_chars: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if max_chars == 0 {
        return String::new();
    }
    if text.width() <= max_chars {
        return text.to_string();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }

    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        width += ch.width().unwrap_or(0);
        if width > max_chars - 3 {
            break;
        }
        out.push(ch);
    }
    out.push_str("...");
    out
}

fn split_sentence(line: &str) -> Option<(&str, &str)> {
    for (index, ch) in line.char_indices() {
        if matches!(ch, '.' | '!' | '?') {
            let rest = &line[index + ch.len_utf8()..];
            if !rest.starts_with(char::is_whitespace) {
                continue;
            }
            let next = rest.trim_start();
            if !next.is_empty() {
                return Some((&line[..=index], next));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sentence_prefers_sentence_break() {
        let text = "Do the parser pass. Then rerun tests.";
        assert_eq!(first_sentence_or_line(text), "Do the parser pass.");
    }

    #[test]
    fn scanline_summary_crops_long_text() {
        let text =
            "This is a long summary line that should crop once it crosses the configured width.";
        assert_eq!(scanline_summary(text, 8, 24), "This is a long summar...");
    }

    #[test]
    fn sentence_keeps_filenames_and_versions() {
        assert_eq!(
            first_sentence_or_line("Fixed src/main.rs for 0.1.0. Tests pass."),
            "Fixed src/main.rs for 0.1.0."
        );
    }

    #[test]
    fn crop_respects_terminal_cells() {
        use unicode_width::UnicodeWidthStr;
        let text = "界界界界界 wide text";
        for width in 0..20 {
            assert!(crop_chars(text, width).width() <= width);
        }
    }
}
