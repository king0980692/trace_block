use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn str_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Greedy word wrap to `width` display columns. Words longer than a line are hard-split.
/// Always returns at least one (possibly empty) line.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out = Vec::new();
    for para in text.split('\n') {
        let para = para.replace('\t', "    ");
        let mut line = String::new();
        let mut line_w = 0usize;
        for word in split_keep_spaces(&para) {
            let w = str_width(word);
            if line_w + w <= width {
                line.push_str(word);
                line_w += w;
                continue;
            }
            if word.trim().is_empty() {
                // whitespace that would overflow: break here, drop it
                out.push(std::mem::take(&mut line));
                line_w = 0;
                continue;
            }
            if line_w > 0 && w <= width {
                out.push(std::mem::take(&mut line).trim_end().to_string());
                line.push_str(word);
                line_w = w;
                continue;
            }
            // hard split a long word
            for ch in word.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                if line_w + cw > width {
                    out.push(std::mem::take(&mut line));
                    line_w = 0;
                }
                line.push(ch);
                line_w += cw;
            }
        }
        out.push(line);
    }
    out
}

fn split_keep_spaces(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_space = None;
    for (i, ch) in s.char_indices() {
        let sp = ch == ' ';
        match in_space {
            Some(prev) if prev != sp => {
                parts.push(&s[start..i]);
                start = i;
            }
            _ => {}
        }
        in_space = Some(sp);
    }
    if start < s.len() {
        parts.push(&s[start..]);
    }
    parts
}

/// Truncate to `width` display columns, appending `…` when cut.
pub fn trunc(s: &str, width: usize) -> String {
    if str_width(s) <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Collapse all whitespace runs (incl. newlines) into single spaces.
pub fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn human_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / 1048576.0)
    }
}

pub fn human_dur(secs: f64) -> String {
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        format!("{}m{:02}s", (secs / 60.0) as u64, (secs % 60.0) as u64)
    }
}

/// `[image · image/png · 8.0 KB]` for an image content block — pi `{type:"image", mimeType, data}` or
/// Anthropic `{type:"image", source:{media_type, data}}`; the size is the decoded length of the
/// recorded base64 data. None for any other block.
pub fn image_placeholder(b: &serde_json::Value) -> Option<String> {
    if b["type"] != "image" {
        return None;
    }
    let mime = b["mimeType"]
        .as_str()
        .or_else(|| b["source"]["media_type"].as_str())
        .unwrap_or("?");
    let data = b["data"].as_str().or_else(|| b["source"]["data"].as_str());
    let size = match data {
        Some(d) => {
            let pad = d.bytes().rev().take_while(|&c| c == b'=').count();
            format!(" · {}", human_bytes((d.len() / 4 * 3).saturating_sub(pad)))
        }
        None if b["source"]["url"].is_string() => format!(" · {}", b["source"]["url"].as_str().unwrap_or("")),
        None => String::new(),
    };
    Some(format!("[image · {mime}{size}]"))
}

/// All readable content of a message/tool content value (a string or content blocks): text as is,
/// images as placeholders, other blocks as compact JSON.
pub fn content_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(a) => a
            .iter()
            .filter_map(|b| {
                b["text"]
                    .as_str()
                    .map(String::from)
                    .or_else(|| image_placeholder(b))
                    .or_else(|| (!b.is_null()).then(|| b.to_string()))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// `12 lines · 3.4 KB` for tool output; image placeholders are counted as images, not as text:
/// `1 lines · 20 B · 1 image`, or just `1 image`.
pub fn output_meta(body: &str) -> String {
    let images = body.lines().filter(|l| l.starts_with("[image · ")).count();
    let text: Vec<&str> = body.lines().filter(|l| !l.starts_with("[image · ")).collect();
    let bytes: usize = text.iter().map(|l| l.len()).sum::<usize>() + text.len().saturating_sub(1);
    let mut parts = Vec::new();
    if !text.is_empty() || images == 0 {
        parts.push(format!("{} lines · {}", text.len(), human_bytes(bytes)));
    }
    if images > 0 {
        parts.push(format!("{images} image{}", if images == 1 { "" } else { "s" }));
    }
    parts.join(" · ")
}

/// Content of a `{"content":[…]}` tool result (text parts, images as placeholders).
pub fn result_text(v: &serde_json::Value) -> String {
    content_text(&v["content"])
}

/// Short human form of tool args: the bash command verbatim; otherwise the main path/query
/// followed by the remaining fields as `key=value`.
pub fn args_summary(args: &serde_json::Value) -> String {
    let Some(obj) = args.as_object() else {
        return serde_json::to_string(args).unwrap_or_default();
    };
    let mut head = None;
    for key in ["command", "path", "file_path", "pattern", "query", "url"] {
        if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
            head = Some((key, s.to_string()));
            break;
        }
    }
    let mut parts: Vec<String> = head.iter().map(|(_, s)| s.clone()).collect();
    for (k, v) in obj {
        if head.as_ref().is_some_and(|(hk, _)| hk == k) {
            continue;
        }
        let v = match v {
            serde_json::Value::String(s) => s.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        };
        parts.push(format!("{k}={v}"));
    }
    parts.join("  ")
}

/// Pretty-print text that is a JSON object/array; return other text unchanged.
pub fn pretty_if_json(text: &str) -> String {
    let t = text.trim_start();
    if (t.starts_with('{') || t.starts_with('['))
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(text)
        && (v.is_object() || v.is_array())
    {
        return serde_json::to_string_pretty(&v).unwrap_or_else(|_| text.to_string());
    }
    text.to_string()
}

/// Highlighted tool-name badge; each tool keeps a stable background color.
pub fn badge(name: &str, color: bool) -> String {
    if !color {
        return format!("[{name}]");
    }
    let bg = match name {
        "bash" | "shell" => "43",
        "read" => "42",
        "write" | "edit" | "multiedit" | "patch" => "45",
        "grep" | "find" | "ls" | "glob" | "search" => "44",
        _ => ["46", "41", "43", "42", "45", "44"][name.bytes().map(|b| b as usize).sum::<usize>() % 6],
    };
    format!("\x1b[1;30;{bg}m {name} \x1b[0m")
}

/// Prominent model badge (white on magenta).
pub fn model_badge(model: &str, color: bool) -> String {
    if color {
        format!("\x1b[1;97;45m {model} \x1b[0m")
    } else {
        format!("<{model}>")
    }
}

/// `bash ×3  read ×1` with badges; failures in red.
pub fn tool_list(stats: &[(String, usize, usize)], color: bool) -> String {
    stats
        .iter()
        .map(|(n, c, f)| {
            let mut s = format!("{} ×{c}", badge(n, color));
            if *f > 0 {
                s += &if color {
                    format!("\x1b[1;31m ({f} failed)\x1b[0m")
                } else {
                    format!(" ({f} failed)")
                };
            }
            s
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// Names of the tools offered in a system `message_start`.
pub fn offered_tools(msg: &serde_json::Value) -> Vec<String> {
    msg["toolsAdded"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| t["name"].as_str().map(String::from)).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_basic() {
        assert_eq!(wrap("hello world foo", 11), vec!["hello world", "foo"]);
        assert_eq!(wrap("", 10), vec![""]);
        assert_eq!(wrap("a\nb", 10), vec!["a", "b"]);
        assert_eq!(wrap("abcdefghijkl", 8), vec!["abcdefgh", "ijkl"]);
    }

    #[test]
    fn args_and_json() {
        let a = serde_json::json!({"path": "./corpus/Book3.txt", "offset": 17160, "limit": 200});
        assert_eq!(args_summary(&a), "./corpus/Book3.txt  offset=17160  limit=200");
        assert_eq!(args_summary(&serde_json::json!({"command": "ls -la"})), "ls -la");
        assert_eq!(pretty_if_json("{\"a\":1}"), "{\n  \"a\": 1\n}");
        assert_eq!(pretty_if_json("plain text"), "plain text");
    }

    #[test]
    fn image_blocks_become_placeholders() {
        let pi = serde_json::json!({"type": "image", "mimeType": "image/png", "data": "iVBORw0KGgo="});
        assert_eq!(image_placeholder(&pi).unwrap(), "[image · image/png · 8 B]");
        let anth = serde_json::json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "AAAA"}});
        assert_eq!(image_placeholder(&anth).unwrap(), "[image · image/jpeg · 3 B]");
        let c = serde_json::json!([{"type": "text", "text": "look:"}, pi]);
        assert_eq!(content_text(&c), "look:\n[image · image/png · 8 B]");
    }

    #[test]
    fn output_meta_counts_images_separately() {
        assert_eq!(output_meta("a\nbc"), "2 lines · 4 B");
        assert_eq!(output_meta("[image · image/png · 8.0 KB]"), "1 image");
        assert_eq!(
            output_meta("Read image file\n[image · image/png · 8.0 KB]"),
            "1 lines · 15 B · 1 image"
        );
    }

    #[test]
    fn trunc_basic() {
        assert_eq!(trunc("hello", 10), "hello");
        assert_eq!(trunc("hello world", 6), "hello…");
    }
}
