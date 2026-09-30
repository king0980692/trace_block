//! Minimal streaming-friendly Markdown → ANSI renderer for the assistant's answer text.
//! Re-run on the whole text at every delta; unclosed markers simply style to end of line.

use unicode_width::UnicodeWidthChar;

const BOLD: u8 = 1;
const ITALIC: u8 = 2;
const CODE: u8 = 4;
const HEAD: u8 = 8;
const QUOTE: u8 = 16;
const BLOCK: u8 = 32;
const MARK: u8 = 64; // bullet glyphs / rules

fn sgr(st: u8) -> String {
    let mut codes = Vec::new();
    if st & HEAD != 0 {
        codes.push("1;33");
    }
    if st & BOLD != 0 {
        codes.push("1");
    }
    if st & ITALIC != 0 {
        codes.push("3");
    }
    if st & (CODE | BLOCK) != 0 {
        codes.push("36");
    }
    if st & QUOTE != 0 {
        codes.push("2");
    }
    if st & MARK != 0 {
        codes.push("34");
    }
    if codes.is_empty() {
        "\x1b[0m".into()
    } else {
        format!("\x1b[0;{}m", codes.join(";"))
    }
}

type Cells = Vec<(char, u8)>;

/// Strip inline markers (`**`, `*`, `` ` ``) and record styles per visible char.
fn inline(line: &str, base: u8) -> Cells {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::with_capacity(chars.len());
    let (mut bold, mut ital, mut code) = (false, false, false);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let prev = if i > 0 { Some(chars[i - 1]) } else { None };
        if c == '`' {
            code = !code;
            i += 1;
            continue;
        }
        if !code && c == '*' && next == Some('*') {
            bold = !bold;
            i += 2;
            continue;
        }
        if !code && c == '*' {
            let opens = !ital && next.is_some_and(|n| !n.is_whitespace());
            let closes = ital && prev.is_some_and(|p| !p.is_whitespace());
            if opens || closes {
                ital = !ital;
                i += 1;
                continue;
            }
        }
        let mut st = base;
        if bold {
            st |= BOLD;
        }
        if ital {
            st |= ITALIC;
        }
        if code {
            st |= CODE;
        }
        out.push((c, st));
        i += 1;
    }
    out
}

fn cw(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Greedy word wrap of styled cells; continuation lines get `hang` spaces.
fn wrap_cells(cells: &Cells, width: usize, hang: usize) -> Vec<Cells> {
    let width = width.max(10);
    let mut lines: Vec<Cells> = Vec::new();
    let mut line: Cells = Vec::new();
    let mut w = 0usize;
    let mut i = 0;
    let indent = |n: usize| -> Cells { vec![(' ', 0); n] };
    while i < cells.len() {
        // next token: a run of spaces or a run of non-spaces
        let sp = cells[i].0 == ' ';
        let mut j = i;
        while j < cells.len() && (cells[j].0 == ' ') == sp {
            j += 1;
        }
        let tok = &cells[i..j];
        let tw: usize = tok.iter().map(|c| cw(c.0)).sum();
        if w + tw <= width {
            line.extend_from_slice(tok);
            w += tw;
        } else if sp {
            lines.push(std::mem::take(&mut line));
            line = indent(hang);
            w = hang;
        } else if tw <= width - hang && w > hang {
            while line.last().is_some_and(|c| c.0 == ' ') {
                line.pop();
            }
            lines.push(std::mem::take(&mut line));
            line = indent(hang);
            line.extend_from_slice(tok);
            w = hang + tw;
        } else {
            for &c in tok {
                if w + cw(c.0) > width {
                    lines.push(std::mem::take(&mut line));
                    line = indent(hang);
                    w = hang;
                }
                line.push(c);
                w += cw(c.0);
            }
        }
        i = j;
    }
    lines.push(line);
    lines
}

fn to_ansi(cells: &Cells) -> String {
    let mut s = String::new();
    let mut cur = 0u8;
    for &(c, st) in cells {
        if st != cur {
            s += &sgr(st);
            cur = st;
        }
        s.push(c);
    }
    if cur != 0 {
        s += "\x1b[0m";
    }
    s
}

/// Render markdown `text` to display lines (ANSI-styled), each at most `width` columns.
pub fn render(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_block = false;
    for raw in text.split('\n') {
        let line = raw.replace('\t', "    ");
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_block = !in_block;
            let lang = trimmed.trim_start_matches('`').trim();
            let label = if in_block && !lang.is_empty() {
                format!("┌─ {lang}")
            } else if in_block {
                "┌─".into()
            } else {
                "└─".into()
            };
            out.push(to_ansi(&label.chars().map(|c| (c, BLOCK)).collect()));
            continue;
        }
        if in_block {
            let cells: Cells = "│ ".chars().chain(line.chars()).map(|c| (c, BLOCK)).collect();
            out.extend(wrap_cells(&cells, width, 2).iter().map(to_ansi));
            continue;
        }
        // horizontal rule
        let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
        if compact.len() >= 3 && (compact.chars().all(|c| c == '-') || compact.chars().all(|c| c == '*')) {
            out.push(to_ansi(&"─".repeat(width.min(60)).chars().map(|c| (c, MARK)).collect()));
            continue;
        }
        // heading
        if let Some(h) = trimmed.strip_prefix('#') {
            let body = h.trim_start_matches('#').trim();
            let cells = inline(body, HEAD);
            out.extend(wrap_cells(&cells, width, 0).iter().map(to_ansi));
            continue;
        }
        // blockquote
        if let Some(q) = trimmed.strip_prefix('>') {
            let mut cells: Cells = "▌ ".chars().map(|c| (c, MARK)).collect();
            cells.extend(inline(q.trim_start(), QUOTE));
            out.extend(wrap_cells(&cells, width, 2).iter().map(to_ansi));
            continue;
        }
        // bullets / numbered lists (keep nesting indent)
        let lead = line.len() - trimmed.len();
        let bullet = ["- ", "* ", "+ "].iter().find(|b| trimmed.starts_with(**b));
        let numbered = trimmed
            .find(". ")
            .filter(|&i| i > 0 && i <= 3 && trimmed[..i].chars().all(|c| c.is_ascii_digit()));
        if bullet.is_some() || numbered.is_some() {
            let (glyph, rest) = match (bullet, numbered) {
                (Some(_), _) => ("•".to_string(), &trimmed[2..]),
                (None, Some(i)) => (trimmed[..=i].to_string(), &trimmed[i + 2..]),
                _ => unreachable!(),
            };
            let mut cells: Cells = vec![(' ', 0); lead];
            cells.extend(glyph.chars().map(|c| (c, MARK)));
            cells.push((' ', 0));
            let hang = lead + glyph.chars().map(cw).sum::<usize>() + 1;
            cells.extend(inline(rest, 0));
            out.extend(wrap_cells(&cells, width, hang).iter().map(to_ansi));
            continue;
        }
        let cells = inline(&line, 0);
        out.extend(wrap_cells(&cells, width, 0).iter().map(to_ansi));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[String]) -> Vec<String> {
        let re = |s: &str| {
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
        };
        lines.iter().map(|l| re(l)).collect()
    }

    #[test]
    fn strips_markers_and_styles() {
        let l = render("Hello **bold** and *it* `x`", 80);
        assert_eq!(plain(&l), vec!["Hello bold and it x"]);
        assert!(l[0].contains("\x1b[0;1m"));
    }

    #[test]
    fn bullets_hang() {
        let l = plain(&render(
            "- **Killed by Bellatrix** during the battle of the department",
            30,
        ));
        assert_eq!(l[0], "• Killed by Bellatrix during");
        assert!(l[1].starts_with("  the"));
    }

    #[test]
    fn heading_rule_code() {
        let l = plain(&render("### **Summary**\n---\n```sh\nls\n```", 40));
        assert_eq!(l[0], "Summary");
        assert!(l[1].starts_with("───"));
        assert_eq!(l[2], "┌─ sh");
        assert_eq!(l[3], "│ ls");
    }

    #[test]
    fn unclosed_marker_while_streaming() {
        assert_eq!(plain(&render("partial **bol", 40)), vec!["partial bol"]);
        assert_eq!(plain(&render("5 * 3 = 15", 40)), vec!["5 * 3 = 15"]);
    }
}
