//! `trace_block replay FILE.jsonl` — re-emit a recorded trace on stdout with realistic pacing,
//! to test the live renderer / viewer without running pi or a provider.

use anyhow::{Context, Result, bail};
use std::io::{BufRead, Write};
use std::time::Duration;

pub const USAGE: &str = "\
trace_block replay — stream a recorded trace (pi or Claude Code) with realistic timing (test producer)

USAGE:
  trace_block replay FILE.jsonl [--speed X]

  --speed X   time multiplier; 2 = twice as fast, 0.5 = slower (default 1)

Pacing: ~25 ms per text/thinking delta, 1.5 s per tool call, 0.6 s per turn,
retry waits use the event's delayMs. A full fixture takes ~1–2 minutes at speed 1.

EXAMPLE (two panes):
  A: trace_block replay tests/data/pi-synthetic.jsonl | trace_block --mmd /tmp/trace.mmd >/dev/null
  B: trace_block view /tmp/trace.mmd
";

fn delay_after(line: &str) -> f64 {
    let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) else {
        return 0.05;
    };
    match ev["type"].as_str().unwrap_or("") {
        "message_update" => match ev["assistantMessageEvent"]["type"].as_str().unwrap_or("") {
            "text_delta" | "thinking_delta" | "toolcall_delta" => 0.025,
            _ => 0.1,
        },
        "tool_execution_start" => 1.5,
        "tool_execution_update" => 0.2,
        "turn_start" => 0.6,
        "auto_retry_start" => ev["delayMs"].as_f64().unwrap_or(2000.0) / 1000.0,
        "session" => 0.5,
        // Claude Code stream-json
        "stream_event" => match ev["event"]["type"].as_str().unwrap_or("") {
            "content_block_delta" => 0.025,
            _ => 0.05,
        },
        "assistant" if ev["message"]["content"][0]["type"] == "tool_use" => 1.5,
        _ => 0.05,
    }
}

pub fn run(args: &[String]) -> Result<()> {
    let mut file = None;
    let mut speed = 1.0f64;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            "--speed" => speed = it.next().context("--speed needs a value")?.parse().context("--speed")?,
            p if !p.starts_with('-') && file.is_none() => file = Some(p.to_string()),
            other => bail!("replay: unknown argument '{other}' (see `trace_block replay --help`)"),
        }
    }
    let file = file.context("replay: missing FILE.jsonl (see `trace_block replay --help`)")?;
    if speed <= 0.0 {
        bail!("--speed must be > 0");
    }
    let f = std::fs::File::open(&file).with_context(|| format!("opening {file}"))?;
    let mut reader = std::io::BufReader::new(f);
    let mut out = std::io::stdout().lock();
    loop {
        let mut buf = Vec::new();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        if out.write_all(&buf).and_then(|_| out.flush()).is_err() {
            break; // consumer went away
        }
        let d = delay_after(&String::from_utf8_lossy(&buf)) / speed;
        std::thread::sleep(Duration::from_secs_f64(d));
    }
    Ok(())
}
