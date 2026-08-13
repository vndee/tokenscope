# Plan Quota Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show each account's remaining plan quota in the Tokenscope panel, and warn from the tray before a limit is hit, so checking usage no longer means opening the Claude and ChatGPT apps.

**Architecture:** Both agents produce the same `QuotaSnapshot`. Codex's comes free — `rate_limits` rides on the `token_count` lines the Codex parser already reads, and is carried through the ingest manifest into the store. Claude persists nothing locally, so a separate poller shells out to `claude -p "/usage"` per account (via `CLAUDE_CONFIG_DIR`) on a slow timer and caches the parsed result. Neither path touches a credential.

**Tech Stack:** Rust (Tauri 2 backend), React + TypeScript frontend, `serde`, `std::process`. No new crates.

## Global Constraints

- Run Rust commands from `src-tauri/`; frontend commands from the repo root.
- Spec: `docs/superpowers/specs/2026-08-13-plan-quota-design.md`. Read it before starting.
- **No credential handling.** Never read the macOS Keychain, `~/.claude/.credentials.json`, or `~/.codex/auth.json`. Never call an undocumented HTTP endpoint. Both data paths are a local file read and a supported CLI invocation.
- A quota figure older than **30 minutes** is stale: it dims in the UI and never triggers the tray warning.
- Tray warning threshold: **80%**, taken as the highest `used_percent` across every account and every window.
- Unparseable input yields **no window**, never a guessed number. Zero windows means the whole snapshot is `None`.
- `model.rs` serializes multi-word fields to camelCase with `#[serde(rename)]` — follow that convention.
- Commit after each task, with the message given in that task's final step.

## File Structure

| File | Responsibility |
|---|---|
| `src-tauri/src/model.rs` (modify) | `QuotaWindow`, `QuotaSnapshot`, `AccountData.quota` |
| `src-tauri/src/agents/codex.rs` (modify) | Extract `rate_limits` into a snapshot, carry it per file |
| `src-tauri/src/store.rs` (modify) | Carry an opaque newest-quota blob per account; stays agent-agnostic |
| `src-tauri/src/agents/mod.rs` (modify) | `FileState::quota()` hook |
| `src-tauri/src/quota.rs` (create) | Claude `/usage` parsing, binary resolution, poller, cache |
| `src-tauri/src/parser.rs` (modify) | Attach each account's snapshot in `build_workspace` |
| `src-tauri/src/lib.rs` (modify) | Module decl, poller thread, tray warning |
| `src/data.ts` (modify) | TS mirrors of the quota types |
| `src/App.tsx` (modify) | Quota block per tab |

---

### Task 1: Quota types and Codex extraction

**Files:**
- Modify: `src-tauri/src/model.rs`, `src-tauri/src/agents/mod.rs`, `src-tauri/src/store.rs`, `src-tauri/src/agents/codex.rs`

**Interfaces:**
- Produces:
  - `model::QuotaWindow { label: String, used_percent: f64, resets_at: Option<i64>, resets_label: String }` (Serialize + Deserialize + Clone + Debug)
  - `model::QuotaSnapshot { plan: String, windows: Vec<QuotaWindow>, fetched_at: i64, source_at: i64 }` (same derives)
  - `agents::FileState::quota(&self) -> Option<(i64, serde_json::Value)>` — default `None`
  - `Store::quota: Option<(i64, serde_json::Value)>` — public field, newest by timestamp
  - `STORE_VERSION` becomes 9

- [ ] **Step 1: Write the failing test**

Add to `src-tauri/src/agents/codex.rs`'s existing `mod tests`:

```rust
    fn rl(ts: &str, used: f64, window: u64, resets: i64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":10,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":1,"total_tokens":11}}}},"rate_limits":{{"limit_id":"codex","primary":{{"used_percent":{used},"window_minutes":{window},"resets_at":{resets}}},"secondary":null,"plan_type":"pro"}}}}}}"#
        )
    }

    #[test]
    fn rate_limits_become_a_quota_snapshot() {
        let p = CodexParser;
        let mut st = p.new_file_state(None);
        for l in [META, CTX, rl("2026-08-12T04:00:02.000Z", 12.5, 10080, 1787196735).as_str()] {
            st.parse_line(l);
        }
        let (ts, v) = st.quota().expect("a quota snapshot");
        let q: crate::model::QuotaSnapshot = serde_json::from_value(v).unwrap();
        assert_eq!(q.plan, "pro");
        assert_eq!(q.windows.len(), 1);
        assert_eq!(q.windows[0].label, "Week");
        assert_eq!(q.windows[0].used_percent, 12.5);
        assert_eq!(q.windows[0].resets_at, Some(1787196735));
        assert_eq!(ts, 1786507202000); // the event's own timestamp, in ms
    }

    #[test]
    fn the_newest_rate_limits_wins_and_a_null_one_does_not_erase_it() {
        let p = CodexParser;
        let mut st = p.new_file_state(None);
        let null_rl = r#"{"timestamp":"2026-08-12T04:00:09.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":99,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":9,"total_tokens":108}},"rate_limits":null}}"#;
        for l in [
            META,
            CTX,
            rl("2026-08-12T04:00:02.000Z", 5.0, 10080, 1).as_str(),
            rl("2026-08-12T04:00:05.000Z", 9.0, 10080, 2).as_str(),
            null_rl,
        ] {
            st.parse_line(l);
        }
        let (_, v) = st.quota().expect("a quota snapshot");
        let q: crate::model::QuotaSnapshot = serde_json::from_value(v).unwrap();
        assert_eq!(q.windows[0].used_percent, 9.0);
    }

    #[test]
    fn a_secondary_window_is_captured_too() {
        let two = r#"{"timestamp":"2026-08-12T04:00:02.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":1,"total_tokens":11}},"rate_limits":{"limit_id":"codex","primary":{"used_percent":3.0,"window_minutes":300,"resets_at":10},"secondary":{"used_percent":7.0,"window_minutes":10080,"resets_at":20},"plan_type":"plus"}}}"#;
        let p = CodexParser;
        let mut st = p.new_file_state(None);
        for l in [META, CTX, two] {
            st.parse_line(l);
        }
        let (_, v) = st.quota().unwrap();
        let q: crate::model::QuotaSnapshot = serde_json::from_value(v).unwrap();
        assert_eq!(q.plan, "plus");
        assert_eq!(q.windows.len(), 2);
        assert_eq!(q.windows[0].label, "5h");
        assert_eq!(q.windows[1].label, "Week");
    }

    #[test]
    fn quota_survives_a_carry_round_trip() {
        let p = CodexParser;
        let mut st = p.new_file_state(None);
        for l in [META, CTX, rl("2026-08-12T04:00:02.000Z", 4.0, 10080, 7).as_str()] {
            st.parse_line(l);
        }
        let saved = st.carry().unwrap();
        let st2 = p.new_file_state(Some(&saved));
        let (_, v) = st2.quota().expect("quota must survive the manifest round-trip");
        let q: crate::model::QuotaSnapshot = serde_json::from_value(v).unwrap();
        assert_eq!(q.windows[0].used_percent, 4.0);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cd src-tauri
cargo test --lib rate_limits_become 2>&1 | tail -20
```

Expected: FAIL to compile — `st.quota()` and `crate::model::QuotaSnapshot` do not exist.

- [ ] **Step 3: Add the shared types**

In `src-tauri/src/model.rs`, change the import line to `use serde::{Deserialize, Serialize};` and append:

```rust
/// One rolling limit window on a plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub label: String, // "5h" / "Week" / "Session" / "Week (all models)"
    #[serde(rename = "usedPercent")]
    pub used_percent: f64,
    /// Unix seconds, when the source gives a machine timestamp — Codex does.
    /// None for Claude, whose CLI prints only a human string with no year.
    #[serde(rename = "resetsAt")]
    pub resets_at: Option<i64>,
    /// Human reset text for display, e.g. "Aug 20 at 12:59am". Empty if absent.
    #[serde(rename = "resetsLabel", default)]
    pub resets_label: String,
}

/// An account's plan quota as of a point in time. `source_at` is when the data
/// was true; `fetched_at` is when we observed it. They differ for Codex, whose
/// figures come from the last logged event and can be days old.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    pub plan: String,
    pub windows: Vec<QuotaWindow>,
    #[serde(rename = "fetchedAt")]
    pub fetched_at: i64,
    #[serde(rename = "sourceAt")]
    pub source_at: i64,
}
```

- [ ] **Step 4: Add the `FileState::quota` hook and the store's opaque slot**

In `src-tauri/src/agents/mod.rs`, add to the `FileState` trait:

```rust
    /// Newest agent-reported quota seen in this file: (source timestamp ms, a
    /// serialized `model::QuotaSnapshot`). Opaque here so `store.rs` stays
    /// agent-agnostic. Default None for agents that report no quota.
    fn quota(&self) -> Option<(i64, serde_json::Value)> {
        None
    }
```

In `src-tauri/src/store.rs`, add a public field to `Store` and persist it. Find the struct that is serialized as the single store document and add:

```rust
    /// Newest quota blob across this account's files, by source timestamp.
    /// Opaque to the store; `parser.rs` deserializes it.
    #[serde(default)]
    pub quota: Option<(i64, serde_json::Value)>,
```

In `ingest`, after the per-file line loop and before the manifest write, fold the file's quota in:

```rust
            if let Some((ts, v)) = state.quota() {
                if self.quota.as_ref().map(|(prev, _)| ts > *prev).unwrap_or(true) {
                    self.quota = Some((ts, v));
                    dirty = true;
                }
            }
```

Bump the version so the first run rescans and picks up quota already present in existing logs — without it, an account whose files have not changed since the upgrade would show no quota until its next session:

```rust
//   v9: capture agent-reported plan quota during ingest.
const STORE_VERSION: u32 = 9;
```

- [ ] **Step 5: Extract `rate_limits` in the Codex parser**

In `src-tauri/src/agents/codex.rs`, add to `Carry`:

```rust
    /// Newest rate_limits seen in this file: (event ts_ms, snapshot).
    quota: Option<(i64, crate::model::QuotaSnapshot)>,
```

Add the conversion, above `impl CodexState`:

```rust
/// One `primary`/`secondary` limit block → a window. `window_minutes` names it:
/// 10080 is a week, 300 is Codex's 5-hour window; anything else is reported in
/// hours rather than invented.
fn quota_window(v: &Value) -> Option<QuotaWindow> {
    let used = v.get("used_percent").and_then(|x| x.as_f64())?;
    let mins = v.get("window_minutes").and_then(|x| x.as_u64()).unwrap_or(0);
    let label = match mins {
        10080 => "Week".to_string(),
        300 => "5h".to_string(),
        0 => "Limit".to_string(),
        m if m % 60 == 0 => format!("{}h", m / 60),
        m => format!("{m}m"),
    };
    let resets_at = v.get("resets_at").and_then(|x| x.as_i64());
    Some(QuotaWindow { label, used_percent: used, resets_at, resets_label: String::new() })
}

/// A `rate_limits` payload → a snapshot. None when it carries no usable window,
/// so a null or unrecognised shape never overwrites a good earlier reading.
fn quota_from_rate_limits(v: &Value, source_at: i64) -> Option<QuotaSnapshot> {
    let mut windows = Vec::new();
    for key in ["primary", "secondary"] {
        if let Some(w) = v.get(key).filter(|x| !x.is_null()).and_then(quota_window) {
            windows.push(w);
        }
    }
    if windows.is_empty() {
        return None;
    }
    Some(QuotaSnapshot {
        plan: v.get("plan_type").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        windows,
        fetched_at: source_at,
        source_at,
    })
}
```

Add `use crate::model::{QuotaSnapshot, QuotaWindow};` to the file's imports.

In `on_token_count`, before the existing delta work, capture the quota — note this happens even when the token delta is zero, and even inside a skipped fork replay is fine because a newer reading always wins:

```rust
        if let Some(rl) = p.get("rate_limits").filter(|v| !v.is_null()) {
            if let Some(q) = quota_from_rate_limits(rl, ts_ms) {
                if self.c.quota.as_ref().map(|(prev, _)| ts_ms >= *prev).unwrap_or(true) {
                    self.c.quota = Some((ts_ms, q));
                }
            }
        }
```

Implement the trait method on `CodexState`, inside `impl FileState for CodexState`:

```rust
    fn quota(&self) -> Option<(i64, Value)> {
        let (ts, q) = self.c.quota.as_ref()?;
        Some((*ts, serde_json::to_value(q).ok()?))
    }
```

- [ ] **Step 6: Run the tests to verify they pass**

```bash
cd src-tauri
cargo test 2>&1 | tail -6
```

Expected: PASS, all tests including the four new ones.

- [ ] **Step 7: Verify against real Codex logs**

```bash
cd src-tauri
cargo run --example dump >/dev/null 2>&1
python3 -c "
import json
d=json.load(open('$HOME/Library/Caches/tokenscope/store-_Users_vndee__codex.json'))
print('quota slot:', json.dumps(d.get('quota'), indent=2)[:400])
"
```

Expected: a `[timestamp, {plan, windows, ...}]` pair with `plan: "pro"` and one `Week` window. If it is `null`, the fold in Step 4 is not running — check that `STORE_VERSION` was bumped so a rescan actually happened.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/model.rs src-tauri/src/agents/mod.rs src-tauri/src/store.rs src-tauri/src/agents/codex.rs
git commit -m "feat: capture Codex plan quota during ingest"
```

---

### Task 2: Parse the Claude `/usage` output

Pure string parsing, no process spawning. This is the fragile half of the feature, so the tests lock the exact observed format.

**Files:**
- Create: `src-tauri/src/quota.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod quota;` after `mod pricing;`)

**Interfaces:**
- Consumes: `model::{QuotaSnapshot, QuotaWindow}` from Task 1.
- Produces: `quota::parse_usage(out: &str, now_ms: i64) -> Option<QuotaSnapshot>`

- [ ] **Step 1: Write the failing test**

Create `src-tauri/src/quota.rs` containing only:

```rust
// Claude plan quota. Claude Code persists no quota locally, but its supported
// CLI prints it: `claude -p "/usage"`. We shell out per account rather than
// touching the Keychain token or any undocumented endpoint.
use crate::model::{QuotaSnapshot, QuotaWindow};

#[cfg(test)]
mod tests {
    use super::*;

    // Verbatim output from `claude -p "/usage"` on 2026-08-13.
    const REAL: &str = "You are currently using your subscription to power your Claude Code usage\n\nCurrent session: 15% used · resets Aug 13 at 2:09pm (Asia/Saigon)\nCurrent week (all models): 2% used · resets Aug 20 at 12:59am (Asia/Saigon)\nCurrent week (Fable): 0% used\n\nWhat's contributing to your limits usage?\nLast 24h · 2395 requests · 3 sessions\n";

    #[test]
    fn parses_the_real_three_window_output() {
        let q = parse_usage(REAL, 1_700_000_000_000).expect("a snapshot");
        assert_eq!(q.windows.len(), 3);

        assert_eq!(q.windows[0].label, "session");
        assert_eq!(q.windows[0].used_percent, 15.0);
        assert_eq!(q.windows[0].resets_label, "Aug 13 at 2:09pm");
        // Claude prints no year, so we never invent a machine timestamp.
        assert_eq!(q.windows[0].resets_at, None);

        assert_eq!(q.windows[1].label, "week (all models)");
        assert_eq!(q.windows[1].used_percent, 2.0);
        assert_eq!(q.windows[1].resets_label, "Aug 20 at 12:59am");

        // The third window has no reset clause at all.
        assert_eq!(q.windows[2].label, "week (Fable)");
        assert_eq!(q.windows[2].used_percent, 0.0);
        assert_eq!(q.windows[2].resets_label, "");

        assert_eq!(q.plan, "subscription");
        assert_eq!(q.fetched_at, 1_700_000_000_000);
        assert_eq!(q.source_at, 1_700_000_000_000);
    }

    #[test]
    fn an_unknown_extra_window_is_kept_not_dropped() {
        let out = "Current session: 1% used\nCurrent month (something new): 42% used · resets Sep 1 at 9am (UTC)\n";
        let q = parse_usage(out, 0).unwrap();
        assert_eq!(q.windows.len(), 2);
        assert_eq!(q.windows[1].label, "month (something new)");
        assert_eq!(q.windows[1].used_percent, 42.0);
    }

    #[test]
    fn a_decimal_percentage_parses() {
        let q = parse_usage("Current session: 12.5% used\n", 0).unwrap();
        assert_eq!(q.windows[0].used_percent, 12.5);
    }

    #[test]
    fn output_with_no_recognisable_window_is_none() {
        assert!(parse_usage("", 0).is_none());
        assert!(parse_usage("Login required.\n", 0).is_none());
        assert!(parse_usage("Current session: lots used\n", 0).is_none());
    }

    #[test]
    fn a_non_subscription_preamble_is_reported_as_unknown_plan() {
        let q = parse_usage("Using API credits.\n\nCurrent session: 3% used\n", 0).unwrap();
        assert_eq!(q.plan, "");
        assert_eq!(q.windows.len(), 1);
    }
}
```

Add `mod quota;` to `src-tauri/src/lib.rs` directly after the `mod pricing;` line.

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cd src-tauri
cargo test --lib quota:: 2>&1 | tail -20
```

Expected: FAIL to compile — `parse_usage` does not exist.

- [ ] **Step 3: Implement the parser**

Insert above `mod tests` in `src-tauri/src/quota.rs`:

```rust
/// Parse one `Current <label>: <pct>% used[ · resets <when>]` line.
///
/// Hand-rolled rather than regex: the crate has no regex dependency and this
/// shape is small enough to split. Anything that does not match yields None,
/// so an unrecognised line contributes no window instead of a guessed number.
fn parse_window(line: &str) -> Option<QuotaWindow> {
    let rest = line.trim().strip_prefix("Current ")?;
    let (label, rest) = rest.split_once(": ")?;
    let (pct, rest) = rest.split_once("% used")?;
    let used_percent: f64 = pct.trim().parse().ok()?;

    // Optional " · resets Aug 20 at 12:59am (Asia/Saigon)" tail. The timezone
    // parenthetical is dropped; the rest is shown verbatim. No year is printed,
    // so converting to a unix timestamp would mean guessing one.
    let resets_label = rest
        .split_once("resets ")
        .map(|(_, when)| when.split(" (").next().unwrap_or(when).trim().to_string())
        .unwrap_or_default();

    Some(QuotaWindow {
        label: label.trim().to_string(),
        used_percent,
        resets_at: None,
        resets_label,
    })
}

/// Parse the whole `claude -p "/usage"` stdout. `now_ms` is both `fetched_at`
/// and `source_at`: unlike Codex's log-derived figures, the CLI reports live.
pub fn parse_usage(out: &str, now_ms: i64) -> Option<QuotaSnapshot> {
    let windows: Vec<QuotaWindow> = out.lines().filter_map(parse_window).collect();
    if windows.is_empty() {
        return None;
    }
    let plan = if out.contains("using your subscription") {
        "subscription".to_string()
    } else {
        String::new()
    };
    Some(QuotaSnapshot { plan, windows, fetched_at: now_ms, source_at: now_ms })
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cd src-tauri
cargo test 2>&1 | tail -6
```

Expected: PASS, all tests.

- [ ] **Step 5: Check the parser against live output**

```bash
cd /tmp && claude -p "/usage" 2>&1 | sed -n '1,6p'
```

Compare what you see against the `REAL` fixture in the test. If the live format has drifted from the fixture, **stop and report it** — that is the exact failure this feature is most exposed to, and the fixture must be updated deliberately rather than loosened.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/quota.rs src-tauri/src/lib.rs
git commit -m "feat: parse Claude plan quota from the usage CLI output"
```

---

### Task 3: Poll Claude accounts for quota

**Files:**
- Modify: `src-tauri/src/quota.rs`

**Interfaces:**
- Consumes: `parse_usage` from Task 2.
- Produces:
  - `quota::claude_binary() -> Option<PathBuf>`
  - `quota::fetch_claude(config_dir: &Path) -> Option<QuotaSnapshot>`
  - `quota::cached(account_id: &str) -> Option<QuotaSnapshot>`
  - `quota::refresh_claude_accounts()` — sequential over all Claude accounts, fills the cache

- [ ] **Step 1: Write the failing test**

Add to `src-tauri/src/quota.rs`'s `mod tests`:

```rust
    #[test]
    fn the_cache_round_trips_a_snapshot() {
        let q = parse_usage(REAL, 42).unwrap();
        store_cached("acct-test", q);
        let got = cached("acct-test").expect("a cached snapshot");
        assert_eq!(got.windows.len(), 3);
        assert_eq!(got.fetched_at, 42);
    }

    #[test]
    fn an_unknown_account_has_no_cached_snapshot() {
        assert!(cached("acct-never-written").is_none());
    }

    #[test]
    fn binary_resolution_prefers_path_then_known_locations() {
        // Whatever this machine has, resolution must be deterministic and must
        // never shell out through a login shell to find it.
        let a = claude_binary();
        let b = claude_binary();
        assert_eq!(a, b);
        if let Some(p) = a {
            assert!(p.is_absolute(), "resolved path must be absolute: {p:?}");
        }
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cd src-tauri
cargo test --lib quota:: 2>&1 | tail -20
```

Expected: FAIL to compile — `store_cached`, `cached`, `claude_binary` do not exist.

- [ ] **Step 3: Implement resolution, fetching, and the cache**

Add to the top imports of `src-tauri/src/quota.rs`:

```rust
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
```

Append above `mod tests`:

```rust
/// Locate the `claude` binary without a login shell. A GUI app launched at
/// login inherits a minimal PATH, so PATH alone is not enough; the native
/// install's launcher and versioned binaries are checked as fallbacks.
pub fn claude_binary() -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let c = dir.join("claude");
            if c.is_file() {
                return Some(c);
            }
        }
    }
    let home = dirs::home_dir()?;
    let launcher = home.join(".local/bin/claude");
    if launcher.is_file() {
        return Some(launcher);
    }
    // Newest versioned native build, e.g. ~/.local/share/claude/versions/2.1.229
    let versions = home.join(".local/share/claude/versions");
    let mut found: Vec<PathBuf> = std::fs::read_dir(versions)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    found.sort();
    found.pop()
}

/// Run `claude -p "/usage"` for one account and parse the result.
///
/// `config_dir` is the account's data directory, passed as `CLAUDE_CONFIG_DIR`
/// so each account reports its own figures. Returns None on any failure —
/// missing binary, timeout, non-zero exit, or unparseable output — so a bad run
/// never replaces a good cached reading with a wrong one.
pub fn fetch_claude(config_dir: &Path) -> Option<QuotaSnapshot> {
    let bin = claude_binary()?;
    let mut child = std::process::Command::new(bin)
        .env("CLAUDE_CONFIG_DIR", config_dir)
        .arg("-p")
        .arg("/usage")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()
        .ok()?;

    // ~4.5s is typical; 30s is a generous ceiling before we give up and kill it
    // so a wedged child can never pin the poller thread forever.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_usage(&String::from_utf8_lossy(out.stdout.as_slice()), crate::now_ms())
}

static CACHE: OnceLock<Mutex<HashMap<String, QuotaSnapshot>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<String, QuotaSnapshot>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn store_cached(account_id: &str, snap: QuotaSnapshot) {
    if let Ok(mut g) = cache().lock() {
        g.insert(account_id.to_string(), snap);
    }
}

pub fn cached(account_id: &str) -> Option<QuotaSnapshot> {
    cache().lock().ok()?.get(account_id).cloned()
}

/// Refresh every Claude account, one at a time. Sequential on purpose: each run
/// costs several seconds of CPU, and a background menu-bar app should not spawn
/// N of them at once. A failed account keeps whatever was cached before.
///
/// MUST run on a background thread — never the main thread or a BUILD_LOCK
/// holder.
pub fn refresh_claude_accounts() {
    for (d, a) in crate::agents::discover_all() {
        if d.id != "claude" {
            continue;
        }
        // log_root is <data dir>/projects; CLAUDE_CONFIG_DIR wants the data dir.
        let Some(dir) = a.log_root.parent() else { continue };
        if let Some(snap) = fetch_claude(dir) {
            store_cached(&a.id, snap);
        }
    }
}
```

`crate::now_ms()` already exists in `lib.rs`; make it visible to this module by changing its declaration there from `fn now_ms()` to `pub(crate) fn now_ms()`.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cd src-tauri
cargo test 2>&1 | tail -6
```

Expected: PASS, all tests.

- [ ] **Step 5: Verify a real fetch, per account**

Add this test to `src-tauri/src/quota.rs`'s `mod tests`. It spawns real
processes, so it is `#[ignore]`d and only ever run by hand:

```rust
    #[test]
    #[ignore] // spawns `claude` per account; run deliberately, not in CI
    fn live_fetch_each_claude_account() {
        for (d, a) in crate::agents::discover_all() {
            if d.id != "claude" {
                continue;
            }
            let dir = a.log_root.parent().unwrap();
            let q = fetch_claude(dir);
            println!(
                "{} -> {:?}",
                a.label,
                q.map(|x| x
                    .windows
                    .iter()
                    .map(|w| format!("{} {}%", w.label, w.used_percent))
                    .collect::<Vec<_>>())
            );
        }
    }
```

Then run it:

```bash
cd src-tauri
cargo test --lib -- --ignored quota::tests::live_fetch_each_claude_account --exact --nocapture 2>&1 | tail -12
```

Expected: one line per Claude account, each listing its windows and percentages. Cross-check one against `CLAUDE_CONFIG_DIR=<that dir> claude -p "/usage"` run by hand. Keep the test — mark it `#[ignore]` since it spawns processes.

Note the fully-qualified path in the command: with `--exact`, a bare test name matches nothing and exits 0 silently.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/quota.rs src-tauri/src/lib.rs
git commit -m "feat: fetch Claude plan quota per account via the usage CLI"
```

---

### Task 4: Attach quota to the workspace and start the poller

**Files:**
- Modify: `src-tauri/src/model.rs`, `src-tauri/src/parser.rs`, `src-tauri/src/lib.rs`

**Interfaces:**
- Consumes: `Store::quota`, `quota::cached`, `quota::refresh_claude_accounts`.
- Produces: `AccountData.quota: Option<QuotaSnapshot>`, serialized as `quota`.

- [ ] **Step 1: Add the field**

In `src-tauri/src/model.rs`, add to `AccountData`:

```rust
    /// Plan quota for this account, when known. `None` renders as absence — the
    /// UI must never show it as zero.
    pub quota: Option<QuotaSnapshot>,
```

- [ ] **Step 2: Populate it in `build_workspace`**

In `src-tauri/src/parser.rs`, `account_events` currently discards the store after computing events. Return the account's quota alongside by changing its signature to return a 4-tuple, adding this before the `(events, cfg.mcp_servers, cfg.skills)` return:

```rust
    // Codex reports quota in its logs; Claude's arrives from the poller cache.
    let quota = match d.id {
        "claude" => crate::quota::cached(&a.id),
        _ => store
            .quota
            .as_ref()
            .and_then(|(ts, v)| {
                serde_json::from_value::<crate::model::QuotaSnapshot>(v.clone())
                    .ok()
                    .map(|mut q| {
                        q.source_at = *ts;
                        q
                    })
            }),
    };
```

Return `(events, cfg.mcp_servers, cfg.skills, quota)` and update both call sites. In `build_workspace`, pass it into the struct:

```rust
        accounts.push(AccountData {
            id: a.id,
            label: a.label,
            email: a.email,
            agent: a.agent.to_string(),
            quota,
            dash,
        });
```

In `build_period`, bind the extra element to `_` — that path returns a single report, not account data.

- [ ] **Step 3: Start the poller thread**

In `src-tauri/src/lib.rs`'s `setup`, alongside the existing background threads, add:

```rust
            // Claude quota poller. Spawning `claude -p "/usage"` costs several
            // seconds per account, so this runs far slower than the 30s
            // dashboard poll and never on the main thread. The first pass is
            // immediate so the panel has data soon after launch.
            {
                let handle = app.handle().clone();
                std::thread::spawn(move || loop {
                    quota::refresh_claude_accounts();
                    refresh(&handle);
                    std::thread::sleep(Duration::from_secs(300));
                });
            }
```

- [ ] **Step 4: Verify it compiles and the workspace carries quota**

```bash
cd src-tauri
cargo test 2>&1 | tail -4
cargo check --message-format short 2>&1 | tail -3
```

Expected: all tests pass, 0 errors.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/model.rs src-tauri/src/parser.rs src-tauri/src/lib.rs
git commit -m "feat: expose per-account plan quota in the workspace API"
```

---

### Task 5: Show quota in the panel

**Files:**
- Modify: `src/data.ts`, `src/App.tsx`

**Interfaces:**
- Consumes: `AccountData.quota` from Task 4.
- Produces: `QuotaBlock` component; `isStale(sourceAt)` helper exported from `data.ts`.

- [ ] **Step 1: Add the TypeScript types**

In `src/data.ts`, above `AccountData`:

```ts
export interface QuotaWindow { label: string; usedPercent: number; resetsAt: number | null; resetsLabel: string }
export interface QuotaSnapshot { plan: string; windows: QuotaWindow[]; fetchedAt: number; sourceAt: number }
// A quota figure older than this is shown dimmed and never drives the tray warning.
export const QUOTA_STALE_MS = 30 * 60 * 1000;
export const isQuotaStale = (sourceAt: number) => Date.now() - sourceAt > QUOTA_STALE_MS;
```

Extend the interface and the browser-dev fallback:

```ts
export interface AccountData { id: string; label: string; email: string; agent: string; quota: QuotaSnapshot | null; dash: Dashboard }
```

In `fetchWorkspace`'s non-Tauri fallback, add `quota: null` to the synthesised account.

- [ ] **Step 2: Verify the typecheck fails where quota must be threaded**

```bash
npx tsc --noEmit 2>&1 | head -20
```

Expected: an error on the dev-fallback account literal if you missed it. Fix, then confirm clean.

- [ ] **Step 3: Add the quota block**

In `src/App.tsx`, add next to the other small components:

```tsx
// Plan quota for the selected account. Hidden entirely when unknown — showing
// an unknown quota as 0% would read as "plenty left", the most costly possible
// misreading. A figure older than QUOTA_STALE_MS dims, because a stale quota
// presented as current is worse than none.
function QuotaBlock({ t, q }: { t: Theme; q: QuotaSnapshot | null }) {
  if (!q || q.windows.length === 0) return null;
  const stale = isQuotaStale(q.sourceAt);
  const mins = Math.max(0, Math.round((Date.now() - q.sourceAt) / 60000));
  const age = mins < 1 ? "just now" : mins < 60 ? `${mins}m ago` : `${Math.round(mins / 60)}h ago`;
  return (
    <div style={{ opacity: stale ? 0.45 : 1, marginBottom: 12 }}>
      <div style={{ display: "flex", alignItems: "baseline", justifyContent: "space-between", marginBottom: 7 }}>
        <Label t={t}>Plan usage{q.plan ? ` · ${q.plan}` : ""}</Label>
        <span style={{ font: `500 9px ${t.mono}`, color: t.faint }}>as of {age}</span>
      </div>
      {q.windows.map((w) => (
        <div key={w.label} style={{ marginBottom: 6 }}>
          <div style={{ display: "flex", justifyContent: "space-between", font: `500 10px ${t.mono}`, color: t.dim, marginBottom: 3 }}>
            <span>{w.label}</span>
            <span>
              <span style={{ color: w.usedPercent >= 80 ? "#e0795f" : t.text, fontWeight: 600 }}>{w.usedPercent}%</span>
              {w.resetsLabel ? <span style={{ color: t.faint }}> · resets {w.resetsLabel}</span> : null}
            </span>
          </div>
          <div style={{ height: 5, borderRadius: 3, background: t.gridLine, overflow: "hidden" }}>
            <div style={{ width: `${Math.min(100, Math.max(0, w.usedPercent))}%`, height: "100%", borderRadius: 3, background: w.usedPercent >= 80 ? "#e0795f" : t.accent }} />
          </div>
        </div>
      ))}
    </div>
  );
}
```

Add `QuotaSnapshot` and `isQuotaStale` to the existing `./data` import.

Thread the selected account's quota into `Panel`: add `quota: QuotaSnapshot | null` to its props type and destructuring, pass `quota={...}` from `App` (the aggregate "All" tab passes `null` — a combined quota across accounts is not a meaningful number), and render `<QuotaBlock t={t} q={quota} />` immediately above the `SectionRule` that precedes "Tokens by model".

- [ ] **Step 4: Verify**

```bash
npx tsc --noEmit && echo "TYPECHECK OK"
```

Expected: `TYPECHECK OK`.

- [ ] **Step 5: Commit**

```bash
git add src/data.ts src/App.tsx
git commit -m "feat: show plan usage per account in the panel"
```

---

### Task 6: Warn from the tray

**Files:**
- Modify: `src-tauri/src/lib.rs`

**Interfaces:**
- Consumes: `Workspace.accounts[].quota`.
- Produces: `quota_alert(ws: &Workspace) -> Option<(String, String, f64)>` — (account label, window label, percent)

- [ ] **Step 1: Write the failing test**

Add to `src-tauri/src/lib.rs`'s existing `mod tests`:

```rust
    fn snap(source_at: i64, pcts: &[(&str, f64)]) -> model::QuotaSnapshot {
        model::QuotaSnapshot {
            plan: "pro".into(),
            windows: pcts
                .iter()
                .map(|(l, p)| model::QuotaWindow {
                    label: (*l).into(),
                    used_percent: *p,
                    resets_at: None,
                    resets_label: String::new(),
                })
                .collect(),
            fetched_at: source_at,
            source_at,
        }
    }

    #[test]
    fn the_highest_window_across_accounts_drives_the_alert() {
        let now = now_ms();
        let a = snap(now, &[("Session", 20.0), ("Week", 85.0)]);
        let b = snap(now, &[("Week", 40.0)]);
        let got = pick_alert(&[("work".to_string(), Some(a)), ("home".to_string(), Some(b))], now);
        assert_eq!(got, Some(("work".into(), "Week".into(), 85.0)));
    }

    #[test]
    fn nothing_below_the_threshold_alerts() {
        let now = now_ms();
        let a = snap(now, &[("Week", 79.9)]);
        assert!(pick_alert(&[("work".to_string(), Some(a))], now).is_none());
    }

    #[test]
    fn a_stale_snapshot_never_alerts() {
        let now = now_ms();
        // 31 minutes old — past the staleness threshold even though it is high.
        let old = snap(now - 31 * 60 * 1000, &[("Week", 99.0)]);
        assert!(pick_alert(&[("work".to_string(), Some(old))], now).is_none());
    }

    #[test]
    fn accounts_without_quota_are_skipped() {
        let now = now_ms();
        assert!(pick_alert(&[("work".to_string(), None)], now).is_none());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cd src-tauri
cargo test --lib the_highest_window 2>&1 | tail -20
```

Expected: FAIL to compile — `pick_alert` does not exist.

- [ ] **Step 3: Implement the selection and wire it to the tray**

Add to `src-tauri/src/lib.rs`:

```rust
/// Quota at or above this share of a window is worth surfacing on the tray.
const QUOTA_WARN_PERCENT: f64 = 80.0;
/// Matches the UI's staleness rule: a figure older than this never alerts,
/// because the user would act on a number that is no longer true.
const QUOTA_STALE_MS: i64 = 30 * 60 * 1000;

/// The single most urgent quota across every account and window: (account
/// label, window label, percent). None when nothing is high, or everything
/// high is stale.
fn pick_alert(
    accounts: &[(String, Option<model::QuotaSnapshot>)],
    now: i64,
) -> Option<(String, String, f64)> {
    let mut best: Option<(String, String, f64)> = None;
    for (label, q) in accounts {
        let Some(q) = q else { continue };
        if now - q.source_at > QUOTA_STALE_MS {
            continue;
        }
        for w in &q.windows {
            if w.used_percent < QUOTA_WARN_PERCENT {
                continue;
            }
            if best.as_ref().map(|(_, _, p)| w.used_percent > *p).unwrap_or(true) {
                best = Some((label.clone(), w.label.clone(), w.used_percent));
            }
        }
    }
    best
}
```

In `refresh`, replace the tray label/tooltip block with:

```rust
    if let Some(tray) = app.tray_by_id("main") {
        let label = fmt_tokens_m(ws.today_tokens);
        let pairs: Vec<(String, Option<model::QuotaSnapshot>)> = ws
            .accounts
            .iter()
            .map(|a| (a.label.clone(), a.quota.clone()))
            .collect();
        let alert = pick_alert(&pairs, now_ms());
        // macOS renders set_title as plain text and controls its colour, so the
        // warning is a marker glyph rather than a recolour. Windows makes
        // set_title a no-op, which is why the tooltip carries the detail.
        let title = match &alert {
            Some(_) => format!("{label} ⚠"),
            None => label.clone(),
        };
        let tip = match &alert {
            Some((acct, win, pct)) => {
                format!("Tokenscope · today {label}\n{acct}: {win} {pct:.0}% used")
            }
            None => format!("Tokenscope · today {label}"),
        };
        let _ = tray.set_title(Some(title));
        let _ = tray.set_tooltip(Some(tip));
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cd src-tauri
cargo test 2>&1 | tail -6
```

Expected: PASS, all tests.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat: mark the tray when a plan window is nearly spent"
```

---

### Task 7: Documentation and end-to-end verification

**Files:**
- Modify: `README.md`, `README-zh.md`

- [ ] **Step 1: Full check**

```bash
cd src-tauri && cargo test 2>&1 | tail -4 && cargo check --message-format short 2>&1 | tail -3
cd .. && npx tsc --noEmit && echo "TYPECHECK OK"
```

Expected: all tests pass, 0 errors, `TYPECHECK OK`.

- [ ] **Step 2: Verify in the running app**

```bash
pnpm tauri dev
```

Open the panel from the tray icon. Expected:

- Each Claude tab shows a "Plan usage · subscription" block with session and week bars.
- The Codex tab shows a "Plan usage · pro" block with a Week bar.
- Every block carries an "as of …" label.
- The "All" tab shows no quota block.
- Cross-check one Claude tab against `CLAUDE_CONFIG_DIR=<that account dir> claude -p "/usage"` run by hand; the percentages must match.

Report anything that does not match. If the panel cannot be opened in your environment, say so plainly rather than implying it was checked — the tray icon is not clickable by any tool available here.

- [ ] **Step 3: Update the READMEs**

In `README.md`, add to the "What it does" list:

```markdown
- **Plan usage per account** — how much of each Claude and Codex plan window is spent, with reset times, and a `⚠` on the tray when any window passes 80%
```

Add to the "Data sources" table:

```markdown
| Codex plan quota | `rate_limits` on `token_count` lines in the Codex session logs |
| Claude plan quota | `claude -p "/usage"`, run per account with `CLAUDE_CONFIG_DIR` |
```

Add below that table:

```markdown
Plan quota never touches a credential: Codex reports it inside the logs already
being read, and Claude's comes from its own supported CLI. Tokenscope does not
read the Keychain, any auth file, or any undocumented endpoint. Figures older
than 30 minutes are dimmed rather than shown as current.
```

Mirror all of it in `README-zh.md`, keeping the Chinese consistent in tone with the surrounding text.

- [ ] **Step 4: Commit**

```bash
git add README.md README-zh.md
git commit -m "docs: document per-account plan quota"
```

---

## Deliberately not built

**Swapping the tray icon to an amber variant.** The spec lists it as available —
the tray is built with `icon_as_template(false)`, so a coloured image would
render. It is left out because it needs a new hand-designed asset to sit
correctly beside the existing icon at menu-bar size, and the marker glyph plus
tooltip already carry the signal on both platforms. Add it later if the glyph
proves too subtle in practice.

## Notes for the implementer

- **The likeliest thing to break later is the `/usage` text format.** It is not an API. The fixture in Task 2 is deliberately verbatim so a Claude Code release that changes the wording fails the test loudly instead of silently producing wrong percentages.
- Do not convert Claude's reset string to a timestamp. It has no year.
- A quota that cannot be determined must render as absence. Zero means "you have used none", which is the most expensive possible misreading of "unknown".
- An installed `/Applications/Tokenscope.app` may share `~/Library/Caches/tokenscope/` with your dev build. If token totals move between runs, that is why; it is not a quota bug.
