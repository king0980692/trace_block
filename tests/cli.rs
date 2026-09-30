//! End-to-end tests of the `trace_block` binary on small synthetic streams (tests/data/).
//! stderr is a pipe here, so the append-only (plain, no ANSI) view is exercised.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn data(name: &str) -> Vec<u8> {
    std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)).unwrap()
}

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("trace_block-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// Run trace_block with `input` on stdin; returns (stdout, stderr).
fn run(args: &[&str], input: &[u8]) -> (Vec<u8>, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_trace_block"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn trace_block");
    child.stdin.take().unwrap().write_all(input).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "exit status {:?}, stderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    (out.stdout, String::from_utf8_lossy(&out.stderr).into_owned())
}

fn lines_of_types(input: &[u8], keep: impl Fn(&serde_json::Value) -> bool) -> Vec<u8> {
    let mut out = Vec::new();
    for l in input.split_inclusive(|&b| b == b'\n') {
        let v: serde_json::Value = serde_json::from_slice(l).unwrap();
        if keep(&v) {
            out.extend_from_slice(l);
        }
    }
    out
}

#[test]
fn pi_passthrough_is_byte_identical_even_with_malformed_lines() {
    let mut input = data("pi-synthetic.jsonl");
    input.extend_from_slice(b"this is not json\n");
    input.extend_from_slice(b"{\"type\":\"agent_settled\"}"); // no trailing newline
    let (stdout, stderr) = run(&[], &input);
    assert_eq!(stdout, input);
    assert!(stderr.contains("malformed JSON passed through"), "{stderr}");
}

#[test]
fn pi_scroll_view_blocks() {
    let (_, err) = run(&["--scroll"], &data("pi-synthetic.jsonl"));
    for needle in [
        "━━ pi session session-demo",
        "tools available: [bash]",
        "[bash] (command)",
        "▸ user",
        "What is in ./docs?",
        "💭 thinking",
        "I should list the docs directory.",
        "┌ [bash] ls ./docs",
        "└ ✓ [bash]",
        "⚠ retry 1/3",
        "⇄ resp-1 · stop toolUse (tool_calls)",
        "● assistant",
        "== ★ FINAL ANSWER ↑ turn 2 ==",
        "✔ settled",
        "tools used: [bash] ×1",
    ] {
        assert!(err.contains(needle), "missing {needle:?} in:\n{err}");
    }
    assert!(!err.contains('\x1b'), "no ANSI when stderr is not a terminal");
}

#[test]
fn pi_unfinished_stream_is_reported() {
    let input = data("pi-synthetic.jsonl");
    let cut: Vec<u8> = input
        .split_inclusive(|&b| b == b'\n')
        .take(18)
        .flatten()
        .copied()
        .collect();
    let (stdout, err) = run(&["--scroll"], &cut);
    assert_eq!(stdout, cut);
    assert!(err.contains("stream ended without agent_settled"), "{err}");
}

#[test]
fn pi_save_and_trajectory() {
    let input = data("pi-synthetic.jsonl");
    let (raw, jsonl, md) = (tmp("pi-raw.json"), tmp("pi-traj.jsonl"), tmp("pi-traj.md"));
    run(
        &[
            "-q",
            "-o",
            raw.to_str().unwrap(),
            "--trajectory",
            jsonl.to_str().unwrap(),
        ],
        &input,
    );
    run(&["-q", "--trajectory", md.to_str().unwrap()], &input);
    assert_eq!(std::fs::read(&raw).unwrap(), input);
    let expected = lines_of_types(&input, |v| {
        matches!(v["type"].as_str(), Some("session" | "message_end"))
    });
    assert_eq!(std::fs::read(&jsonl).unwrap(), expected);
    let md = std::fs::read_to_string(&md).unwrap();
    for needle in [
        "# pi session session-demo",
        "### system",
        "**toolsAdded**",
        "### user",
        "### assistant",
        "**toolCall** · bash · `call-1`",
        "### toolResult · bash",
    ] {
        assert!(md.contains(needle), "missing {needle:?} in markdown");
    }
}

#[test]
fn claude_stream_is_detected_and_rendered() {
    let input = data("claude-synthetic.jsonl");
    let (stdout, err) = run(&["--scroll"], &input);
    assert_eq!(stdout, input);
    for needle in [
        "━━ claude session 00000000-0000-0000-0000-000000000000",
        "system/init · tools (2)",
        "TURN 1",
        "Find the files.",
        "┌ [Glob] docs/*.txt",
        "└ ✓ [Glob]",
        "stop tool_use",
        "system/thinking_tokens · estimated_tokens 12 (delta 12)",
        "★ FINAL ANSWER",
        "Two files: a.txt and b.txt.",
        "stop end_turn",
        "✔ result success · 1234 ms · 2 turns · $0.0010 · thinking_tokens 12",
    ] {
        assert!(err.contains(needle), "missing {needle:?} in:\n{err}");
    }
}

#[test]
fn claude_trajectory_keeps_message_lines() {
    let input = data("claude-synthetic.jsonl");
    let jsonl = tmp("claude-traj.jsonl");
    run(&["-q", "--trajectory", jsonl.to_str().unwrap()], &input);
    let expected = lines_of_types(&input, |v| {
        matches!(v["type"].as_str(), Some("assistant" | "user" | "result"))
            || (v["type"] == "system" && v["subtype"] == "init")
    });
    assert_eq!(std::fs::read(&jsonl).unwrap(), expected);
}

#[test]
fn replay_reemits_the_file_unchanged() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/pi-synthetic.jsonl");
    let out = Command::new(env!("CARGO_BIN_EXE_trace_block"))
        .args(["replay", path.to_str().unwrap(), "--speed", "1000"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout, data("pi-synthetic.jsonl"));
}

#[test]
fn mermaid_outputs() {
    let (mmd, svg) = (tmp("d.mmd"), tmp("d.svg"));
    run(
        &["-q", "--mmd", mmd.to_str().unwrap(), "--svg", svg.to_str().unwrap()],
        &data("pi-synthetic.jsonl"),
    );
    let src = std::fs::read_to_string(&mmd).unwrap();
    assert!(src.starts_with("sequenceDiagram"), "{src}");
    assert!(src.contains("A->>T0: ls ./docs"), "{src}");
    assert!(std::fs::read_to_string(&svg).unwrap().contains("<svg"));
}

#[test]
fn help_and_bad_args() {
    let out = Command::new(env!("CARGO_BIN_EXE_trace_block"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("USAGE"));
    let out = Command::new(env!("CARGO_BIN_EXE_trace_block"))
        .arg("--nope")
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn pi_session_log_is_rendered() {
    let input = data("pi-session-synthetic.jsonl");
    let (stdout, err) = run(&["--scroll"], &input);
    assert_eq!(stdout, input);
    for needle in [
        "━━ pi session session-demo",
        "model change → example/demo-model",
        "tools available: [bash]",
        "What is in ./docs?",
        "I should list the docs directory.",
        "┌ [bash] ls ./docs",
        "└ ✓ [bash] 2 lines",
        "⇄ LLM response · resp-1 · stop toolUse (tool_calls)",
        "★ FINAL ANSWER",
        "end of session log",
    ] {
        assert!(err.contains(needle), "missing {needle:?} in:\n{err}");
    }
    // no completion marker in a session log is not an error, and no wall-clock timing is shown
    assert!(!err.contains("stream ended without agent_settled"), "{err}");
    assert!(!err.contains("0.0s"), "{err}");
}

#[test]
fn claude_transcript_is_rendered() {
    let input = data("claude-transcript-synthetic.jsonl");
    let (stdout, err) = run(&["--scroll"], &input);
    assert_eq!(stdout, input);
    for needle in [
        "━━ claude session 11111111-1111-1111-1111-111111111111 (transcript)",
        "cwd  /work",
        "▸ user",
        "What is in ./docs?",
        "attachment/prompt_snapshot · system prompt (2 parts)",
        "Find the files.",
        "┌ [Glob] docs/*.txt",
        "stop tool_use",
        "TURN 2 · FINAL",
        "★ FINAL ANSWER",
        "Two files: a.txt and b.txt.",
        "stop end_turn",
        "end of session log",
    ] {
        assert!(err.contains(needle), "missing {needle:?} in:\n{err}");
    }
}

#[test]
fn session_log_trajectory_keeps_message_entries() {
    let input = data("pi-session-synthetic.jsonl");
    let out = tmp("pi-session-traj.jsonl");
    run(&["-q", "--trajectory", out.to_str().unwrap()], &input);
    let expected = lines_of_types(&input, |v| matches!(v["type"].as_str(), Some("session" | "message")));
    assert_eq!(std::fs::read(&out).unwrap(), expected);
}
