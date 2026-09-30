//! Syntax highlighting for pretty-printed JSON in the detail view.

use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy, PartialEq)]
enum Tok {
    Plain,
    Key,
    Str,
    Num,
    Lit,
    Punct,
}

fn sgr(t: Tok) -> &'static str {
    match t {
        Tok::Plain => "\x1b[0m",
        Tok::Key => "\x1b[0;1;36m",
        Tok::Str => "\x1b[0;32m",
        Tok::Num => "\x1b[0;33m",
        Tok::Lit => "\x1b[0;35m",
        Tok::Punct => "\x1b[0;2m",
    }
}

/// Classify every char of one pretty-printed JSON line.
fn tokenize(line: &str) -> Vec<(char, Tok)> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            let start = i;
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' {
                    i += 2;
                    continue;
                }
                if chars[i] == '"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            let end = i.min(chars.len());
            let mut j = end;
            while j < chars.len() && chars[j] == ' ' {
                j += 1;
            }
            let t = if chars.get(j) == Some(&':') { Tok::Key } else { Tok::Str };
            out.extend(chars[start..end].iter().map(|&ch| (ch, t)));
        } else if c == '-' || c.is_ascii_digit() {
            while i < chars.len() && (chars[i].is_ascii_digit() || "-+.eE".contains(chars[i])) {
                out.push((chars[i], Tok::Num));
                i += 1;
            }
        } else if let Some(lit) = ["true", "false", "null"]
            .iter()
            .find(|l| chars[i..].starts_with(&l.chars().collect::<Vec<_>>()))
        {
            for _ in 0..lit.len() {
                out.push((chars[i], Tok::Lit));
                i += 1;
            }
        } else {
            out.push((c, if "{}[],:".contains(c) { Tok::Punct } else { Tok::Plain }));
            i += 1;
        }
    }
    out
}

fn to_ansi(cells: &[(char, Tok)]) -> String {
    let mut s = String::new();
    let mut cur = Tok::Plain;
    for &(c, t) in cells {
        if t != cur {
            s += sgr(t);
            cur = t;
        }
        s.push(c);
    }
    if cur != Tok::Plain {
        s += "\x1b[0m";
    }
    s
}

/// Highlight pretty JSON text; long lines are hard-wrapped to `width`, continuation lines indented
/// two spaces past the line's own indentation.
pub fn highlight(text: &str, width: usize) -> Vec<String> {
    let width = width.max(20);
    let mut out = Vec::new();
    for line in text.lines() {
        let cells = tokenize(line);
        let indent = line.len() - line.trim_start().len();
        let hang = (indent + 2).min(width / 2);
        let mut cur: Vec<(char, Tok)> = Vec::new();
        let mut w = 0;
        for &(c, t) in &cells {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            if w + cw > width {
                out.push(to_ansi(&cur));
                cur = vec![(' ', Tok::Plain); hang];
                w = hang;
            }
            cur.push((c, t));
            w += cw;
        }
        out.push(to_ansi(&cur));
    }
    out
}

/// True when `text` parses as a JSON object or array.
pub fn is_json(text: &str) -> bool {
    let t = text.trim_start();
    (t.starts_with('{') || t.starts_with('[')) && serde_json::from_str::<serde_json::Value>(text).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(s: &str) -> String {
        let mut o = String::new();
        let mut esc = false;
        for c in s.chars() {
            if c == '\x1b' {
                esc = true;
            } else if esc {
                if c == 'm' {
                    esc = false;
                }
            } else {
                o.push(c);
            }
        }
        o
    }

    #[test]
    fn classifies_tokens() {
        let t = tokenize(r#"  "name": "grep", "n": -1.5e3, "ok": true, "x": null"#);
        let kind_at = |needle: &str| {
            let line: String = t.iter().map(|c| c.0).collect();
            t[line.find(needle).unwrap()].1
        };
        assert!(kind_at("\"name\"") == Tok::Key);
        assert!(kind_at("\"grep\"") == Tok::Str);
        assert!(kind_at("-1.5e3") == Tok::Num);
        assert!(kind_at("true") == Tok::Lit);
        assert!(kind_at("null") == Tok::Lit);
    }

    #[test]
    fn escaped_quotes_and_wrap_keep_text() {
        let src = "{\n  \"cmd\": \"grep -i \\\"sirius black\\\" ./corpus/*.txt | head -100\"\n}";
        let lines = highlight(src, 30);
        let joined: String = lines
            .iter()
            .map(|l| plain(l).trim_start().to_string())
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(joined.replace(' ', ""), src.replace(['\n', ' '], ""));
        assert!(
            lines
                .iter()
                .all(|l| unicode_width::UnicodeWidthStr::width(plain(l).as_str()) <= 30)
        );
    }
}
