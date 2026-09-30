//! Character-cell rendering of the sequence diagrams that `diagram.rs` emits, for terminals
//! without image support. Understands exactly the subset of Mermaid we generate.

use crate::util::{trunc, wrap};
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy, PartialEq)]
enum Style {
    Plain,
    Life,
    Head,
    Arrow,
    Reply,
    Error,
    Note,
    Think,
    Warn,
    Good,
}

impl Style {
    fn sgr(self) -> &'static str {
        match self {
            Style::Plain => "0",
            Style::Life => "2",
            Style::Head => "1;36",
            Style::Arrow => "36",
            Style::Reply => "32",
            Style::Error => "1;31",
            Style::Note => "33",
            Style::Think => "2;35",
            Style::Warn => "1;33",
            Style::Good => "1;32",
        }
    }
}

/// One terminal row as cells; `None` marks the second half of a wide char.
struct Row(Vec<(Option<char>, Style)>);

impl Row {
    fn new(width: usize, lifelines: &[usize]) -> Row {
        let mut r = Row(vec![(Some(' '), Style::Plain); width]);
        for &x in lifelines {
            if x < width {
                r.0[x] = (Some('│'), Style::Life);
            }
        }
        r
    }

    fn put(&mut self, x: usize, s: &str, style: Style) {
        let mut x = x;
        for ch in s.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if w == 0 {
                continue;
            }
            if x + w > self.0.len() {
                break;
            }
            self.0[x] = (Some(ch), style);
            if w == 2 {
                self.0[x + 1] = (None, style);
            }
            x += w;
        }
    }

    fn render(&self, color: bool) -> String {
        let mut out = String::new();
        let mut cur = Style::Plain;
        for &(ch, st) in &self.0 {
            let Some(ch) = ch else { continue };
            if color && st != cur {
                out += &format!("\x1b[0;{}m", st.sgr());
                cur = st;
            }
            out.push(ch);
        }
        if color && cur != Style::Plain {
            out += "\x1b[0m";
        }
        out.trim_end().to_string()
    }
}

enum Stmt {
    Msg {
        from: usize,
        to: usize,
        kind: Style,
        dashed: bool,
        text: String,
    },
    Note {
        lo: usize,
        hi: usize,
        text: String,
    },
}

pub struct TextDiagram {
    names: Vec<String>,
    stmts: Vec<Stmt>,
}

pub fn parse(src: &str) -> TextDiagram {
    let mut ids: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut stmts = Vec::new();
    let mut autonumber = false;
    let mut n = 0usize;
    let idx = |id: &str, ids: &mut Vec<String>, names: &mut Vec<String>| -> usize {
        if let Some(i) = ids.iter().position(|x| x == id) {
            return i;
        }
        ids.push(id.to_string());
        names.push(id.to_string());
        ids.len() - 1
    };
    for line in src.lines().map(str::trim) {
        if line == "autonumber" {
            autonumber = true;
        } else if let Some(rest) = line.strip_prefix("participant ") {
            let (id, name) = rest.split_once(" as ").unwrap_or((rest, rest));
            let i = idx(id.trim(), &mut ids, &mut names);
            names[i] = name.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("Note over ") {
            let (who, text) = rest.split_once(':').unwrap_or((rest, ""));
            let parts: Vec<usize> = who.split(',').map(|p| idx(p.trim(), &mut ids, &mut names)).collect();
            let lo = *parts.iter().min().unwrap_or(&0);
            let hi = *parts.iter().max().unwrap_or(&0);
            stmts.push(Stmt::Note {
                lo,
                hi,
                text: text.trim().to_string(),
            });
        } else if let Some((head, text)) = line.split_once(':') {
            for (op, kind, dashed) in [
                ("--x", Style::Error, true),
                ("-->>", Style::Reply, true),
                ("->>", Style::Arrow, false),
            ] {
                if let Some((a, b)) = head.split_once(op) {
                    let from = idx(a.trim(), &mut ids, &mut names);
                    let to = idx(b.trim(), &mut ids, &mut names);
                    n += 1;
                    let text = if autonumber {
                        format!("{n}. {}", text.trim())
                    } else {
                        text.trim().to_string()
                    };
                    stmts.push(Stmt::Msg {
                        from,
                        to,
                        kind,
                        dashed,
                        text,
                    });
                    break;
                }
            }
        }
    }
    TextDiagram { names, stmts }
}

fn note_style(text: &str) -> Style {
    if text.starts_with("thinking:") {
        Style::Think
    } else if text.starts_with("retry") || text.starts_with("model error") || text.starts_with("stream ended") {
        Style::Warn
    } else if text.starts_with("settled") {
        Style::Good
    } else {
        Style::Note
    }
}

impl TextDiagram {
    /// Lane x-positions for a given width.
    fn lanes(&self, width: usize) -> (Vec<usize>, usize) {
        let n = self.names.len().max(1);
        let colw = (width / n).max(12);
        ((0..n).map(|i| i * colw + colw / 2).collect(), colw)
    }

    /// Header rows (participant boxes) and body rows, already styled.
    pub fn render(&self, width: usize, color: bool) -> (Vec<String>, Vec<String>) {
        let (xs, colw) = self.lanes(width);
        let mut header = Vec::new();
        {
            let mut top = Row::new(width, &[]);
            let mut mid = Row::new(width, &[]);
            let mut bot = Row::new(width, &xs);
            for (i, name) in self.names.iter().enumerate() {
                let label = trunc(name, colw.saturating_sub(4).max(4));
                let w = crate::util::str_width(&label) + 2;
                let x0 = xs[i].saturating_sub(w / 2 + 1);
                top.put(x0, &format!("┌{}┐", "─".repeat(w)), Style::Head);
                mid.put(x0, &format!("│ {label} │"), Style::Head);
                bot.put(x0, &format!("└{}┘", "─".repeat(w)), Style::Head);
                bot.put(xs[i], "┬", Style::Head);
            }
            header.extend([top, mid, bot].iter().map(|r| r.render(color)));
        }

        let mut body: Vec<Row> = Vec::new();
        for st in &self.stmts {
            match st {
                Stmt::Msg {
                    from,
                    to,
                    kind,
                    dashed,
                    text,
                } => {
                    let (a, b) = (xs[*from], xs[*to]);
                    let (lo, hi) = (a.min(b), a.max(b));
                    let span = hi - lo;
                    // label sits just above the arrow, wrapped to the span (min 20 cols)
                    let lw = span.saturating_sub(2).max(20).min(width.saturating_sub(lo + 2));
                    let lines = wrap(text, lw);
                    for l in lines.iter().take(2) {
                        let mut r = Row::new(width, &xs);
                        r.put(lo + 2, l, *kind);
                        body.push(r);
                    }
                    let mut r = Row::new(width, &xs);
                    let line = if *dashed { "╌" } else { "─" }.repeat(span.saturating_sub(1));
                    r.put(lo + 1, &line, *kind);
                    let tip = match kind {
                        Style::Error => "✗",
                        _ if b > a => "▶",
                        _ => "◀",
                    };
                    r.put(if b > a { hi - 1 } else { lo + 1 }, tip, *kind);
                    body.push(r);
                }
                Stmt::Note { lo, hi, text } => {
                    let style = note_style(text);
                    let x0 = xs[*lo].saturating_sub(colw / 2 - 1);
                    let x1 = (xs[*hi] + colw / 2 - 1).min(width - 1);
                    let inner = x1.saturating_sub(x0 + 3).max(8);
                    let lines = wrap(text, inner);
                    let mut r = Row::new(width, &xs);
                    r.put(x0, &format!("╭{}╮", "─".repeat(inner + 2)), style);
                    body.push(r);
                    for l in lines.iter().take(3) {
                        let mut r = Row::new(width, &xs);
                        let pad = inner.saturating_sub(crate::util::str_width(l));
                        r.put(x0, &format!("│ {l}{} │", " ".repeat(pad)), style);
                        body.push(r);
                    }
                    let mut r = Row::new(width, &xs);
                    r.put(x0, &format!("╰{}╯", "─".repeat(inner + 2)), style);
                    body.push(r);
                }
            }
        }
        (header, body.iter().map(|r| r.render(color)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_messages_and_notes() {
        let src = "sequenceDiagram\n    autonumber\n    participant U as User\n    participant A as model\n    participant T0 as bash\n    U->>A: hello\n    Note over A: thinking: hmm\n    A->>T0: ls -la\n    T0-->>A: ok · 0.1s\n    T0--xA: ERROR\n    Note over U,A: settled\n";
        let d = parse(src);
        assert_eq!(d.names, vec!["User", "model", "bash"]);
        let (head, body) = d.render(90, false);
        assert!(head[1].contains("User") && head[1].contains("bash"));
        let all = body.join("\n");
        assert!(all.contains("1. hello"));
        assert!(all.contains("▶") && all.contains("◀") && all.contains("✗"));
        assert!(all.contains("thinking: hmm") && all.contains("settled"));
    }
}
