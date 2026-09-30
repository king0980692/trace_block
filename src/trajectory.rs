//! `--trajectory FILE`: the agent's full message sequence, taken from the stream as recorded.
//!
//! `.md`  → readable Markdown of every recorded message (nothing truncated or inferred)
//! other  → JSONL, byte-for-byte as received: for pi the `session` line plus every `message_end`
//!          line (session logs: every `message` entry); for Claude Code the `system/init`,
//!          `assistant`, `user` and `result` lines

use anyhow::{Context, Result};
use serde_json::Value;
use std::fs::File;
use std::io::Write;
use std::path::Path;

enum Format {
    Jsonl,
    Markdown,
}

pub struct Trajectory {
    file: File,
    format: Format,
    turn: usize,
    failed: bool,
    /// Claude Code: last assistant message id seen (a new id starts a new message section)
    claude_msg: Option<String>,
}

fn fence(text: &str) -> String {
    // a fence longer than any backtick run inside the text
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

fn code(v: &mut String, lang: &str, text: &str) {
    let f = fence(text);
    *v += &format!("{f}{lang}\n{}\n{f}\n\n", text.trim_end_matches('\n'));
}

impl Trajectory {
    pub fn create(path: &Path) -> Result<Trajectory> {
        let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        let format = match path.extension().and_then(|e| e.to_str()) {
            Some("md") | Some("markdown") => Format::Markdown,
            _ => Format::Jsonl,
        };
        Ok(Trajectory {
            file,
            format,
            turn: 0,
            failed: false,
            claude_msg: None,
        })
    }

    /// Feed every input line (raw bytes + parsed event).
    pub fn line(&mut self, raw: &[u8], ev: &Value) {
        if self.failed {
            return;
        }
        let res = match self.format {
            Format::Jsonl => self.jsonl(raw, ev),
            Format::Markdown => self.markdown(ev),
        };
        if res.is_err() {
            self.failed = true;
            eprintln!("trace_block: writing --trajectory failed; stopped");
        }
    }

    fn jsonl(&mut self, raw: &[u8], ev: &Value) -> std::io::Result<()> {
        let keep = match ev["type"].as_str() {
            // live streams: session + message_end / Claude assistant, user, result;
            // session logs: pi `message` entries (Claude transcripts reuse assistant/user)
            Some("session") | Some("message_end") | Some("message") | Some("assistant") | Some("user")
            | Some("result") => true,
            Some("system") => ev["subtype"] == "init",
            _ => false,
        };
        if keep {
            self.file.write_all(raw)?;
            if !raw.ends_with(b"\n") {
                self.file.write_all(b"\n")?;
            }
            self.file.flush()?;
        }
        Ok(())
    }

    fn markdown(&mut self, ev: &Value) -> std::io::Result<()> {
        let mut s = String::new();
        match ev["type"].as_str().unwrap_or("") {
            "session" => {
                s += &format!("# pi session {}\n\n", ev["id"].as_str().unwrap_or("?"));
                s += &format!(
                    "- cwd: `{}`\n- timestamp: {}\n\n",
                    ev["cwd"].as_str().unwrap_or("?"),
                    ev["timestamp"].as_str().unwrap_or("?")
                );
            }
            "turn_start" => {
                self.turn += 1;
                s += &format!("---\n\n## turn {}\n\n", self.turn);
            }
            "message_end" | "message" => message_md(&mut s, &ev["message"]),
            "system" if ev["subtype"] == "init" => {
                s += &format!("# claude session {}\n\n", ev["session_id"].as_str().unwrap_or("?"));
                for k in ["cwd", "model", "claude_code_version", "permissionMode", "apiKeySource"] {
                    if let Some(v) = ev[k].as_str() {
                        s += &format!("- {k}: `{v}`\n");
                    }
                }
                s += "\n**system/init** (as recorded)\n\n";
                code(&mut s, "json", &serde_json::to_string_pretty(ev).unwrap_or_default());
            }
            "assistant" => {
                let m = &ev["message"];
                let id = m["id"].as_str().unwrap_or("?").to_string();
                if self.claude_msg.as_deref() != Some(id.as_str()) {
                    self.turn += 1;
                    s += &format!("---\n\n## message {} · `{id}`\n\n", self.turn);
                    for k in ["model", "stop_reason", "request_id"] {
                        let v = if k == "request_id" { &ev[k] } else { &m[k] };
                        if !v.is_null() {
                            s += &format!(
                                "- {k}: `{}`\n",
                                v.as_str().map(String::from).unwrap_or_else(|| v.to_string())
                            );
                        }
                    }
                    s += &format!(
                        "- usage: `{}`\n\n",
                        serde_json::to_string(&m["usage"]).unwrap_or_default()
                    );
                    self.claude_msg = Some(id);
                }
                for b in m["content"].as_array().into_iter().flatten() {
                    match b["type"].as_str().unwrap_or("") {
                        "text" => {
                            s += "**text**\n\n";
                            code(&mut s, "markdown", b["text"].as_str().unwrap_or(""));
                        }
                        "thinking" => {
                            s += "**thinking**\n\n";
                            code(&mut s, "text", b["thinking"].as_str().unwrap_or(""));
                        }
                        "tool_use" | "server_tool_use" => {
                            s += &format!(
                                "**tool_use** · {} · `{}`\n\n",
                                b["name"].as_str().unwrap_or("?"),
                                b["id"].as_str().unwrap_or("?")
                            );
                            code(
                                &mut s,
                                "json",
                                &serde_json::to_string_pretty(&b["input"]).unwrap_or_default(),
                            );
                        }
                        other => {
                            s += &format!("**{other}**\n\n");
                            code(&mut s, "json", &serde_json::to_string_pretty(b).unwrap_or_default());
                        }
                    }
                }
            }
            "user" => {
                for b in ev["message"]["content"].as_array().into_iter().flatten() {
                    if b["type"] == "tool_result" {
                        s += &format!(
                            "### tool_result · `{}` · is_error: {}\n\n",
                            b["tool_use_id"].as_str().unwrap_or("?"),
                            b["is_error"]
                        );
                        let text = crate::util::content_text(&b["content"]);
                        code(&mut s, "text", &text);
                    } else {
                        s += "### user\n\n";
                        code(&mut s, "json", &serde_json::to_string_pretty(b).unwrap_or_default());
                    }
                }
                if let Some(t) = ev["message"]["content"].as_str() {
                    s += "### user\n\n";
                    code(&mut s, "text", t);
                }
            }
            "result" => {
                s += "---\n\n## result\n\n";
                code(&mut s, "json", &serde_json::to_string_pretty(ev).unwrap_or_default());
            }
            _ => return Ok(()),
        }
        self.file.write_all(s.as_bytes())?;
        self.file.flush()
    }
}

fn message_md(s: &mut String, m: &Value) {
    let role = m["role"].as_str().unwrap_or("?");
    match role {
        "system" => {
            *s += "### system\n\n";
            if let Some(c) = m["content"].as_str().filter(|c| !c.is_empty()) {
                code(s, "text", c);
            }
            if let Some(sec) = m["sections"].as_object() {
                for (k, v) in sec {
                    *s += &format!("**{k}**\n\n");
                    code(s, "text", v.as_str().unwrap_or(""));
                }
            }
            if let Some(t) = m
                .get("toolsAdded")
                .filter(|t| t.as_array().is_some_and(|a| !a.is_empty()))
            {
                *s += "**toolsAdded**\n\n";
                code(s, "json", &serde_json::to_string_pretty(t).unwrap_or_default());
            }
            if let Some(t) = m
                .get("toolsRemoved")
                .filter(|t| t.as_array().is_some_and(|a| !a.is_empty()))
            {
                *s += "**toolsRemoved**\n\n";
                code(s, "json", &serde_json::to_string_pretty(t).unwrap_or_default());
            }
        }
        "user" => {
            *s += "### user\n\n";
            content_md(s, &m["content"], "text");
        }
        "assistant" => {
            *s += "### assistant\n\n";
            // every recorded top-level field except content, as a compact list
            if let Some(obj) = m.as_object() {
                for (k, v) in obj {
                    if k == "content" || k == "role" {
                        continue;
                    }
                    let val = match v {
                        Value::String(x) => x.clone(),
                        other => serde_json::to_string(other).unwrap_or_default(),
                    };
                    *s += &format!("- {k}: `{val}`\n");
                }
                *s += "\n";
            }
            content_md(s, &m["content"], "markdown");
        }
        "toolResult" => {
            *s += &format!(
                "### toolResult · {} · `{}` · isError: {}\n\n",
                m["toolName"].as_str().unwrap_or("?"),
                m["toolCallId"].as_str().unwrap_or("?"),
                m["isError"]
            );
            content_md(s, &m["content"], "text");
            if let Some(d) = m.get("details").filter(|d| !d.is_null()) {
                *s += "**details**\n\n";
                code(s, "json", &serde_json::to_string_pretty(d).unwrap_or_default());
            }
        }
        other => {
            *s += &format!("### {other}\n\n");
            code(s, "json", &serde_json::to_string_pretty(m).unwrap_or_default());
        }
    }
}

/// `text_lang`: fence language for text blocks (assistant prose is markdown, tool output is text).
fn content_md(s: &mut String, content: &Value, text_lang: &str) {
    if let Some(text) = content.as_str() {
        if !text.is_empty() {
            code(s, "text", text);
        }
        return;
    }
    for b in content.as_array().into_iter().flatten() {
        match b["type"].as_str().unwrap_or("") {
            "text" => {
                *s += "**text**\n\n";
                code(s, text_lang, b["text"].as_str().unwrap_or(""));
            }
            "thinking" => {
                *s += "**thinking**\n\n";
                code(s, "text", b["thinking"].as_str().unwrap_or(""));
            }
            "toolCall" => {
                *s += &format!(
                    "**toolCall** · {} · `{}`\n\n",
                    b["name"].as_str().unwrap_or("?"),
                    b["id"].as_str().unwrap_or("?")
                );
                code(
                    s,
                    "json",
                    &serde_json::to_string_pretty(&b["arguments"]).unwrap_or_default(),
                );
            }
            "image" => {
                // base64 data is omitted here; the JSONL trajectory / -o file keep it byte-exact
                *s += &format!(
                    "**image** · {} (base64 data omitted)\n\n",
                    crate::util::image_placeholder(b).unwrap_or_default()
                );
            }
            other => {
                *s += &format!("**{other}**\n\n");
                code(s, "json", &serde_json::to_string_pretty(b).unwrap_or_default());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonl_is_a_byte_exact_filter() {
        let dir = std::env::temp_dir().join(format!("tb-traj-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("t.jsonl");
        let mut t = Trajectory::create(&out).unwrap();
        let data = fixture_or_skip!("sirius-trace.jsonl");
        let mut expected = String::new();
        for l in data.split_inclusive('\n') {
            let ev: Value = serde_json::from_str(l.trim_end()).unwrap();
            t.line(l.as_bytes(), &ev);
            if matches!(ev["type"].as_str(), Some("session") | Some("message_end")) {
                expected += l;
            }
        }
        drop(t);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), expected);
        assert_eq!(expected.lines().count(), 1 + 40);
    }

    #[test]
    fn markdown_has_every_message() {
        let dir = std::env::temp_dir().join(format!("tb-traj-md-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("t.md");
        let mut t = Trajectory::create(&out).unwrap();
        for l in fixture_or_skip!("sirius-trace.jsonl").lines() {
            t.line(l.as_bytes(), &serde_json::from_str(l).unwrap());
        }
        drop(t);
        let md = std::fs::read_to_string(&out).unwrap();
        assert_eq!(md.matches("### toolResult").count(), 17);
        assert_eq!(md.matches("**toolCall**").count(), 17);
        assert_eq!(md.matches("### assistant").count(), 21); // every recorded assistant message_end (incl. 3 failed attempts)
        assert!(md.contains("### system") && md.contains("**toolsAdded**") && md.contains("### user"));
        assert!(md.contains("Sirius Black III"));
    }
}

#[cfg(test)]
mod claude_tests {
    use super::*;

    #[test]
    fn claude_jsonl_filter_and_markdown() {
        let dir = std::env::temp_dir().join(format!("tb-traj-claude-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let data = fixture_or_skip!("claude-sirius.jsonl");
        let mut j = Trajectory::create(&dir.join("c.jsonl")).unwrap();
        let mut md = Trajectory::create(&dir.join("c.md")).unwrap();
        let mut expected = String::new();
        for l in data.split_inclusive('\n') {
            let ev: Value = serde_json::from_str(l.trim_end()).unwrap();
            j.line(l.as_bytes(), &ev);
            md.line(l.as_bytes(), &ev);
            let t = ev["type"].as_str().unwrap();
            if matches!(t, "assistant" | "user" | "result") || (t == "system" && ev["subtype"] == "init") {
                expected += l;
            }
        }
        drop(j);
        drop(md);
        assert_eq!(std::fs::read_to_string(dir.join("c.jsonl")).unwrap(), expected);
        let md = std::fs::read_to_string(dir.join("c.md")).unwrap();
        assert_eq!(md.matches("## message ").count(), 5);
        assert_eq!(md.matches("**tool_use**").count(), 4);
        assert_eq!(md.matches("### tool_result").count(), 4);
        assert!(md.contains("# claude session") && md.contains("## result"));
    }
}
