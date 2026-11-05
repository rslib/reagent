//! Footprint budget: a long scripted run must stay under a peak memory and a
//! CPU-per-turn budget.
//!
//! The run uses the scripted fake provider, so it exists only when the
//! `testing` feature is on.
#![cfg(feature = "testing")]

use std::process::Stdio;

use nix::sys::resource::{Usage, UsageWho, getrusage};
use serde_json::{Value, json};
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");
const TURNS: usize = 190;
const TEXT_BYTES: usize = 300;

/// About 2x the release peak, so a leak or an eager copy of history shows.
const MAX_RSS_BYTES: u64 = 16 * 1024 * 1024;
/// About 2x the release CPU cost per turn, which is mostly the `true` child.
const MAX_CPU_MICROS_PER_TURN: u64 = 7_500;
/// Debug builds run far slower, so the CPU budget is loose there.
const DEBUG_CPU_FACTOR: u64 = 10;
/// A debug binary maps far more code: about 17.3 MB peak measured on Linux with
/// codemode built in (16.3 MB before), so the debug budget is looser too.
const MAX_DEBUG_RSS_BYTES: u64 = 24 * 1024 * 1024;

/// Peak resident set size over the waited children, in bytes.
///
/// macOS reports `ru_maxrss` in bytes and Linux in KiB.
fn max_rss_bytes(usage: &Usage) -> u64 {
    let raw = u64::try_from(usage.max_rss()).unwrap_or(0);
    if cfg!(target_os = "macos") {
        raw
    } else {
        raw * 1024
    }
}

fn cpu_micros(usage: &Usage) -> u64 {
    let micros = |time: nix::sys::time::TimeVal| {
        u64::try_from(time.tv_sec()).unwrap_or(0) * 1_000_000
            + u64::try_from(time.tv_usec()).unwrap_or(0)
    };
    micros(usage.user_time()) + micros(usage.system_time())
}

#[tokio::test]
async fn a_long_scripted_run_stays_within_the_footprint_budget() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let text = "x".repeat(TEXT_BYTES);
    let mut replies: Vec<Value> = (0..TURNS)
        .map(|_| json!({"text": text, "calls": [{"name": "bash", "arguments": {"command": "true"}}]}))
        .collect();
    replies.push(json!({"text": "done"}));
    let script = tmp.path().join("script.json");
    tokio::fs::write(&script, json!({"replies": replies}).to_string())
        .await
        .expect("write script");

    let before = getrusage(UsageWho::RUSAGE_CHILDREN).expect("getrusage before");
    let output = Command::new(BIN)
        // The user's real config must not change the test.
        .env("XDG_CONFIG_HOME", "/nonexistent/reagent-test-config")
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .args(["-p", "task", "--json", "--provider"])
        .arg(format!("fake:{}", script.display()))
        .arg("--cwd")
        .arg(tmp.path())
        .env("ANTHROPIC_API_KEY", "sk-ant-test-footprint")
        .stdin(Stdio::null())
        .output()
        .await
        .expect("the binary runs");
    let after = getrusage(UsageWho::RUSAGE_CHILDREN).expect("getrusage after");

    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let summaries = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains(r#""type":"summary""#))
        .count();
    assert_eq!(summaries, 1, "one summary line");

    // Bash children are counted too, so this is the max over all children. In
    // practice that is reagent itself, since `true` is far smaller.
    let rss = max_rss_bytes(&after).max(max_rss_bytes(&before));
    let turns = (TURNS + 1) as u64;
    let cpu_per_turn = (cpu_micros(&after) - cpu_micros(&before)) / turns;
    eprintln!(
        "footprint: peak_rss={rss} bytes ({:.1} MiB), cpu_per_turn={cpu_per_turn} us over {turns} turns",
        rss as f64 / (1024.0 * 1024.0)
    );

    let cpu_budget = if cfg!(debug_assertions) {
        MAX_CPU_MICROS_PER_TURN * DEBUG_CPU_FACTOR
    } else {
        MAX_CPU_MICROS_PER_TURN
    };
    let rss_budget = if cfg!(debug_assertions) {
        MAX_DEBUG_RSS_BYTES
    } else {
        MAX_RSS_BYTES
    };
    assert!(rss <= rss_budget, "peak rss {rss} over {rss_budget}");
    assert!(
        cpu_per_turn <= cpu_budget,
        "cpu per turn {cpu_per_turn} us over {cpu_budget} us"
    );
}
