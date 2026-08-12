# Codex Tracking Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Track OpenAI Codex CLI usage (tokens, cost, models, MCP, Skills) alongside Claude Code, behind a shared agent-adapter registry.

**Architecture:** A new `src-tauri/src/agents/` module owns everything agent-specific: where an agent's logs and whitelists live (`AccountSpec`), and how to turn its JSONL lines into `RawEvent`s (`LogParser`/`FileState`). `store.rs` keeps only the generic incremental-ingest machinery and calls into an injected parser. `parser.rs` iterates the registry instead of `accounts::discover()`. Claude moves onto this abstraction in the same change, so there is exactly one ingest path.

**Tech Stack:** Rust (Tauri 2 backend), React + TypeScript (frontend), `serde_json`, `walkdir`, `chrono`, new `toml` dependency. Rust tests are inline `#[cfg(test)] mod tests` blocks run with `cargo test`. There is no frontend test runner; frontend changes are verified with `npx tsc --noEmit` and the `public/dev-dashboard.json` preview.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-08-12-codex-tracking-design.md`. Read it before starting.
- Run every Rust command from `src-tauri/`; every frontend command from the repo root.
- `STORE_VERSION` ends this change at **7** (currently 6). Bump exactly once, in Task 2.
- Codex token attribution uses the **difference between consecutive `total_token_usage`** values, never the sum of `last_token_usage` (verified: summing deltas over-counts by 550K in a 27.85M session).
- `cached_input_tokens` is a **subset** of `input_tokens`. Uncached input is `input_tokens - cached_input_tokens`.
- Codex `tool_results` and `tool_errors` stay `0` — Codex has no general tool-error flag.
- Never read `~/.codex/auth.json` (holds refresh tokens). Default Codex tab label is `"Codex"`.
- Preserve existing behaviour for Claude. Any Claude-visible change is a bug unless the task says otherwise.
- Commit after each task, with the message given in that task's final step.

## File Structure

| File | Responsibility |
|---|---|
| `src-tauri/src/agents/mod.rs` (create) | `AccountSpec`, `AgentDescriptor`, `LogParser`/`FileState` traits, `slug()`, `registry()`, `discover_all()` |
| `src-tauri/src/agents/claude.rs` (create) | Claude discovery (from `accounts.rs`) + Claude line parsing (from `store.rs`) |
| `src-tauri/src/agents/codex.rs` (create) | Codex discovery, config loading, and stateful log parsing |
| `src-tauri/src/accounts.rs` (delete) | Superseded by `agents/claude.rs` |
| `src-tauri/src/store.rs` (modify) | Generic ingest only: byte offsets, `FileEntry` with `carry`, dedup, prune, cache I/O |
| `src-tauri/src/config.rs` (modify) | Whitelist primitives both agents compose: `.claude.json` MCP, `config.toml` MCP, multi-dir skills |
| `src-tauri/src/parser.rs` (modify) | Iterate the registry; `vendor_of` learns `codex` |
| `src-tauri/src/model.rs` (modify) | `AccountData.agent` |
| `src-tauri/src/lib.rs` (modify) | Module declarations; watcher iterates `log_root` |
| `src-tauri/Cargo.toml` (modify) | Add `toml` |
| `src/data.ts` (modify) | `AccountData.agent` type + dev fallback |
| `src/App.tsx` (modify) | Agent badge on tabs |

---

### Task 1: Agent abstraction with Claude moved onto it

Pure refactor. Claude's discovery and parsing move behind the new traits with **no behavioural change**.

**Files:**
- Create: `src-tauri/src/agents/mod.rs`
- Create: `src-tauri/src/agents/claude.rs`
- Delete: `src-tauri/src/accounts.rs`
- Modify: `src-tauri/src/store.rs` (remove Claude parsing; `ingest` takes a parser)
- Modify: `src-tauri/src/parser.rs` (call sites), `src-tauri/src/lib.rs` (module decls, watcher call site)

**Interfaces:**
- Consumes: existing `RawEvent`, `UserConfig::load_for`.
- Produces:
  - `agents::AccountSpec { id: String, agent: &'static str, label: String, email: String, log_root: PathBuf, config_file: PathBuf, skill_dirs: Vec<PathBuf> }`
  - `agents::FileState` with `fn parse_line(&mut self, line: &str) -> Option<RawEvent>` and `fn carry(&self) -> Option<serde_json::Value>` (default `None`)
  - `agents::LogParser` with `fn new_file_state(&self, carry: Option<&serde_json::Value>) -> Box<dyn FileState>`
  - `agents::AgentDescriptor { id, display, discover: fn() -> Vec<AccountSpec>, load_config: fn(&AccountSpec) -> UserConfig, parser: fn() -> Box<dyn LogParser> }`
  - `agents::registry() -> &'static [AgentDescriptor]`
  - `agents::discover_all() -> Vec<(&'static AgentDescriptor, AccountSpec)>`
  - `agents::slug(&Path) -> String`
  - `agents::claude::DESCRIPTOR`
  - `Store::ingest(&mut self, log_root: &Path, parser: &dyn LogParser) -> bool`

- [ ] **Step 1: Capture a Claude-output baseline to diff against**

```bash
cd src-tauri
cargo run --example dump > /tmp/tokenscope-baseline.json
wc -c /tmp/tokenscope-baseline.json
```

Expected: a non-trivial JSON file (tens of KB). If it is empty or errors, stop — you need a working baseline before refactoring.

- [ ] **Step 2: Create `agents/mod.rs`**

```rust
// Agent adapters. Everything that differs between the CLIs Tokenscope tracks —
// where the logs live, how a log line becomes a RawEvent, which whitelist file
// to read — lives behind these traits, so store.rs stays generic and a new CLI
// is a new file here rather than a new branch everywhere.
pub mod claude;

use crate::config::UserConfig;
use crate::store::RawEvent;
use std::path::{Path, PathBuf};

/// One tracked account: a single agent CLI's config directory.
pub struct AccountSpec {
    /// Slug of the config dir. Namespaces the incremental cache and is the tab key.
    pub id: String,
    /// Owning agent's descriptor id ("claude" / "codex").
    pub agent: &'static str,
    pub label: String,
    pub email: String,
    /// Directory walked for *.jsonl logs.
    pub log_root: PathBuf,
    pub config_file: PathBuf,
    pub skill_dirs: Vec<PathBuf>,
}

/// Per-file parsing state. Created fresh for each source file so streaming
/// state (Codex's cumulative token counter) resets exactly when a file is
/// re-read from byte 0.
pub trait FileState {
    fn parse_line(&mut self, line: &str) -> Option<RawEvent>;
    /// State to persist in the manifest so the next incremental pass over this
    /// same file resumes exactly where this one stopped. `None` = stateless.
    fn carry(&self) -> Option<serde_json::Value> {
        None
    }
}

pub trait LogParser: Send + Sync {
    fn new_file_state(&self, carry: Option<&serde_json::Value>) -> Box<dyn FileState>;
}

pub struct AgentDescriptor {
    pub id: &'static str,
    pub display: &'static str,
    pub discover: fn() -> Vec<AccountSpec>,
    pub load_config: fn(&AccountSpec) -> UserConfig,
    pub parser: fn() -> Box<dyn LogParser>,
}

static REGISTRY: &[AgentDescriptor] = &[claude::DESCRIPTOR];

pub fn registry() -> &'static [AgentDescriptor] {
    REGISTRY
}

/// Every account across every agent, in registry order (Claude first).
pub fn discover_all() -> Vec<(&'static AgentDescriptor, AccountSpec)> {
    let mut out = Vec::new();
    for d in registry() {
        for a in (d.discover)() {
            out.push((d, a));
        }
    }
    out
}

/// A filesystem-safe, stable id derived from a config dir path, so two accounts
/// never share an events cache and a tab key survives restarts.
pub fn slug(p: &Path) -> String {
    p.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}
```

- [ ] **Step 3: Create `agents/claude.rs` by moving existing code**

Move the whole body of `src-tauri/src/accounts.rs` here, and the `parse_line` / `parse_user` / `parse_assistant` / `parse_user_command` / `extract_tag` functions (and any helpers they call) out of `store.rs`. Change only what the new shape requires:

- `label_and_email` and `try_add` keep their logic verbatim.
- `try_add` builds an `AccountSpec` instead of an `Account`:

```rust
fn try_add(out: &mut Vec<AccountSpec>, seen: &mut HashSet<PathBuf>, data_dir: PathBuf, config_file: PathBuf) {
    // Only a directory that actually has a projects/ log dir is an account.
    if !data_dir.join("projects").is_dir() {
        return;
    }
    let canon = std::fs::canonicalize(&data_dir).unwrap_or_else(|_| data_dir.clone());
    if !seen.insert(canon) {
        return;
    }
    let (label, email) = label_and_email(&config_file, &data_dir);
    out.push(AccountSpec {
        id: super::slug(&data_dir),
        agent: "claude",
        label,
        email,
        log_root: data_dir.join("projects"),
        config_file,
        skill_dirs: vec![data_dir.join("skills")],
    });
}
```

- Add the descriptor and a stateless parser wrapper:

```rust
pub const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "claude",
    display: "Claude Code",
    discover,
    load_config,
    parser,
};

fn load_config(a: &AccountSpec) -> UserConfig {
    UserConfig::load_for(&a.config_file, &a.skill_dirs[0])
}

fn parser() -> Box<dyn LogParser> {
    Box::new(ClaudeParser)
}

/// Claude log lines are self-contained, so the file state holds nothing.
struct ClaudeParser;
struct ClaudeState;

impl LogParser for ClaudeParser {
    fn new_file_state(&self, _carry: Option<&serde_json::Value>) -> Box<dyn FileState> {
        Box::new(ClaudeState)
    }
}

impl FileState for ClaudeState {
    fn parse_line(&mut self, line: &str) -> Option<RawEvent> {
        parse_line(line)
    }
}
```

Delete `src-tauri/src/accounts.rs`.

- [ ] **Step 4: Make `Store::ingest` take a parser**

In `store.rs`, change the signature and the two lines that use it:

```rust
    pub fn ingest(&mut self, log_root: &std::path::Path, parser: &dyn crate::agents::LogParser) -> bool {
        let mut dirty = false;
        if !log_root.exists() {
            return false;
        }
        for entry in WalkDir::new(log_root)
```

Inside the per-file loop, after the offset is resolved and before the line loop:

```rust
            let mut state = parser.new_file_state(None);
```

and replace `if let Some(mut ev) = parse_line(s)` with:

```rust
                if let Some(mut ev) = state.parse_line(s) {
```

Remove the now-moved Claude parsing functions from `store.rs`. Keep `RawEvent`, `Manifest`, `Store`, `STORE_VERSION`, `write_atomic`, `cache_dir`.

- [ ] **Step 5: Update `parser.rs` and `lib.rs` call sites**

In `lib.rs`, replace `mod accounts;` with `mod agents;`.

In `parser.rs`, change `account_events` to take the descriptor and spec:

```rust
fn account_events(
    d: &crate::agents::AgentDescriptor,
    a: &crate::agents::AccountSpec,
    pricing: &Pricing,
    cutoff: i64,
) -> (Vec<Event>, HashSet<String>, HashSet<String>) {
    let mut store = Store::load(&a.id);
    let mut dirty = store.ingest(&a.log_root, (d.parser)().as_ref());
    if store.prune_before(cutoff) {
        dirty = true;
    }
    if dirty {
        store.save(&a.id);
    }
    let cfg = (d.load_config)(a);
```

The rest of the function body is unchanged. In `build_workspace` and `build_period`, replace `for a in crate::accounts::discover()` with:

```rust
    for (d, a) in crate::agents::discover_all() {
```

and pass `d` through to `account_events(d, &a, &pricing, cutoff)`.

In `lib.rs`'s watcher thread, replace the `accounts::discover()` loop with:

```rust
                    for (_, a) in agents::discover_all() {
                        let _ = std::fs::create_dir_all(&a.log_root);
                        if watcher.watch(&a.log_root, RecursiveMode::Recursive).is_ok() {
                            watched += 1;
                        }
                    }
```

- [ ] **Step 6: Verify it compiles and Claude output is byte-identical**

```bash
cd src-tauri
cargo test 2>&1 | tail -20
cargo run --example dump > /tmp/tokenscope-after.json
diff /tmp/tokenscope-baseline.json /tmp/tokenscope-after.json && echo "IDENTICAL"
```

Expected: all existing tests pass, and `IDENTICAL` prints. A diff here means the refactor changed Claude behaviour — fix before continuing.

- [ ] **Step 7: Commit**

```bash
git add -A src-tauri/src
git commit -m "refactor: move Claude ingest behind an agent adapter registry"
```

---

### Task 2: Manifest carries per-file parser state

**Files:**
- Modify: `src-tauri/src/store.rs`

**Interfaces:**
- Consumes: `agents::LogParser`, `agents::FileState::carry` from Task 1.
- Produces: `Store::ingest` now threads `carry` through the manifest. `STORE_VERSION == 7`.

- [ ] **Step 1: Write the failing test**

Add to the bottom of `src-tauri/src/store.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{FileState, LogParser};

    /// A parser whose state is a running line count, exposed via carry(). Lets us
    /// assert that state survives a split (incremental) read of the same file.
    struct CountParser;
    struct CountState {
        n: u64,
    }

    impl LogParser for CountParser {
        fn new_file_state(&self, carry: Option<&serde_json::Value>) -> Box<dyn FileState> {
            let n = carry.and_then(|v| v.get("n")).and_then(|v| v.as_u64()).unwrap_or(0);
            Box::new(CountState { n })
        }
    }

    impl FileState for CountState {
        fn parse_line(&mut self, _line: &str) -> Option<RawEvent> {
            self.n += 1;
            None
        }
        fn carry(&self) -> Option<serde_json::Value> {
            Some(serde_json::json!({ "n": self.n }))
        }
    }

    #[test]
    fn carry_survives_an_incremental_read() {
        let dir = std::env::temp_dir().join(format!("ts-carry-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let log = dir.join("a.jsonl");
        fs::write(&log, "one\ntwo\n").unwrap();

        let mut store = Store {
            events: Vec::new(),
            index: HashMap::new(),
            manifest: Manifest::default(),
        };
        store.ingest(&dir, &CountParser);
        let key = log.to_string_lossy().to_string();
        assert_eq!(store.manifest.files[&key].carry, Some(serde_json::json!({ "n": 2 })));

        // Append two more lines; the second pass must resume from 2, not 0.
        fs::write(&log, "one\ntwo\nthree\nfour\n").unwrap();
        store.ingest(&dir, &CountParser);
        assert_eq!(store.manifest.files[&key].carry, Some(serde_json::json!({ "n": 4 })));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncation_clears_carry_and_rereads() {
        let dir = std::env::temp_dir().join(format!("ts-trunc-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let log = dir.join("b.jsonl");
        fs::write(&log, "one\ntwo\nthree\n").unwrap();

        let mut store = Store {
            events: Vec::new(),
            index: HashMap::new(),
            manifest: Manifest::default(),
        };
        store.ingest(&dir, &CountParser);
        let key = log.to_string_lossy().to_string();
        assert_eq!(store.manifest.files[&key].carry, Some(serde_json::json!({ "n": 3 })));

        // Rewrite shorter: the old bytes are gone, so counting restarts at 1.
        fs::write(&log, "x\n").unwrap();
        store.ingest(&dir, &CountParser);
        assert_eq!(store.manifest.files[&key].carry, Some(serde_json::json!({ "n": 1 })));

        let _ = fs::remove_dir_all(&dir);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cd src-tauri
cargo test carry_survives_an_incremental_read 2>&1 | tail -20
```

Expected: FAIL to compile — `manifest.files[&key].carry` does not exist (the entry is still a tuple).

- [ ] **Step 3: Introduce `FileEntry` and thread `carry` through `ingest`**

Replace the `Manifest` definition:

```rust
#[derive(Serialize, Deserialize, Clone, Default)]
struct FileEntry {
    size: u64,
    mtime_ms: i64,
    /// Bytes of this file already ingested.
    offset: u64,
    /// Parser state at that offset, so an incremental read resumes exactly
    /// (Codex diffs a cumulative token counter and cannot restart from zero).
    #[serde(default)]
    carry: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Default)]
struct Manifest {
    files: HashMap<String, FileEntry>,
}
```

Replace the offset-resolution block in `ingest`:

```rust
            let (offset, carry) = match self.manifest.files.get(&key).cloned() {
                Some(e) => {
                    if e.size == size && e.mtime_ms == mtime_ms {
                        continue; // unchanged → skip
                    }
                    if size < e.offset {
                        // truncated / rewritten (e.g. log compaction): the bytes
                        // we already ingested are gone, so purge this file's
                        // events and re-read from the start, idempotently. The
                        // carried parser state described those bytes, so it goes
                        // too — otherwise the rescan diffs against a stale baseline.
                        self.purge_source(&key);
                        (0, None)
                    } else {
                        (e.offset, e.carry)
                    }
                }
                None => (0, None),
            };
            let mut offset = offset;
            let mut state = parser.new_file_state(carry.as_ref());
```

and the manifest write at the end of the loop:

```rust
            offset += process_until as u64;
            self.manifest.files.insert(
                key,
                FileEntry {
                    size,
                    mtime_ms,
                    offset,
                    carry: state.carry(),
                },
            );
            dirty = true;
```

Bump the version and document it:

```rust
//   v6: count tool_result blocks + errors (is_error) from user messages.
//   v7: per-file parser carry in the manifest (Codex cumulative token deltas).
const STORE_VERSION: u32 = 7;
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cd src-tauri
cargo test 2>&1 | tail -20
```

Expected: PASS, including both new tests and every pre-existing test.

- [ ] **Step 5: Verify Claude output is still identical**

```bash
cd src-tauri
cargo run --example dump > /tmp/tokenscope-after2.json
diff /tmp/tokenscope-baseline.json /tmp/tokenscope-after2.json && echo "IDENTICAL"
```

Expected: `IDENTICAL`. (The version bump discards the on-disk cache and rescans; the result must match.)

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/store.rs
git commit -m "feat: persist per-file parser state in the ingest manifest (store v7)"
```

---

### Task 3: Whitelist primitives for both agents

**Files:**
- Modify: `src-tauri/src/config.rs`
- Modify: `src-tauri/Cargo.toml`

**Interfaces:**
- Produces:
  - `config::mcps_from_claude_json(&Path) -> HashSet<String>`
  - `config::mcps_from_codex_toml(&Path) -> HashSet<String>`
  - `config::skills_from_dirs(&[PathBuf]) -> HashSet<String>`
  - `UserConfig::load_for` retained unchanged for Claude.

- [ ] **Step 1: Add the `toml` dependency**

In `src-tauri/Cargo.toml`, under `[dependencies]`, after `serde_json = "1"`:

```toml
# parse ~/.codex/config.toml for the user's Codex MCP server whitelist
toml = "0.8"
```

- [ ] **Step 2: Write the failing test**

Add to the bottom of `src-tauri/src/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_toml_yields_top_level_mcp_server_names() {
        let dir = std::env::temp_dir().join(format!("ts-toml-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let cfg = dir.join("config.toml");
        fs::write(
            &cfg,
            r#"
model = "gpt-5.6-sol"

[mcp_servers.github]
url = "https://api.githubcopilot.com/mcp/"

[mcp_servers.github.http_headers]
Authorization = "Bearer x"

[mcp_servers.playwright]
command = "npx"
args = ["@playwright/mcp@latest"]
"#,
        )
        .unwrap();

        let got = mcps_from_codex_toml(&cfg);
        // Nested tables (github.http_headers) must not become servers.
        assert_eq!(got.len(), 2);
        assert!(got.contains("github"));
        assert!(got.contains("playwright"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_malformed_toml_is_an_empty_whitelist() {
        assert!(mcps_from_codex_toml(Path::new("/nonexistent/config.toml")).is_empty());
    }

    #[test]
    fn skills_come_from_every_listed_dir() {
        let root = std::env::temp_dir().join(format!("ts-skills-{}", std::process::id()));
        let a = root.join("a/skills");
        let b = root.join("b/skills");
        let _ = fs::create_dir_all(a.join("review"));
        let _ = fs::create_dir_all(b.join("payment-integration"));

        let got = skills_from_dirs(&[a, b, root.join("missing")]);
        assert_eq!(got.len(), 2);
        assert!(got.contains("review"));
        assert!(got.contains("payment-integration"));

        let _ = fs::remove_dir_all(&root);
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

```bash
cd src-tauri
cargo test --lib config 2>&1 | tail -20
```

Expected: FAIL to compile — `mcps_from_codex_toml` and `skills_from_dirs` do not exist.

- [ ] **Step 4: Implement the primitives**

In `config.rs`, add `use std::path::PathBuf;` to the imports, rename the private `mcps_from` to `pub fn mcps_from_claude_json(config_file: &Path) -> HashSet<String>` that reads and parses the file itself, and add:

```rust
/// Top-level `[mcp_servers.<name>]` keys from a Codex `config.toml`. Nested
/// tables (`[mcp_servers.github.http_headers]`) are values of their parent, not
/// servers, so iterating the `mcp_servers` table's own keys is exactly right.
pub fn mcps_from_codex_toml(config_file: &Path) -> HashSet<String> {
    let mut set = HashSet::new();
    let Ok(text) = fs::read_to_string(config_file) else {
        return set;
    };
    let Ok(v) = text.parse::<toml::Value>() else {
        return set;
    };
    if let Some(t) = v.get("mcp_servers").and_then(|m| m.as_table()) {
        for k in t.keys() {
            set.insert(k.clone());
        }
    }
    set
}

/// Union of the subdirectory names of every listed skills dir. Missing dirs are
/// simply skipped, so an agent can name roots that may not exist yet.
pub fn skills_from_dirs(dirs: &[PathBuf]) -> HashSet<String> {
    let mut set = HashSet::new();
    for d in dirs {
        scan_skill_dir(d, &mut set);
    }
    set
}
```

Rewrite `UserConfig::load_for` in terms of these so there is one implementation:

```rust
    pub fn load_for(config_file: &Path, skills_dir: &Path) -> Self {
        UserConfig {
            mcp_servers: mcps_from_claude_json(config_file),
            skills: skills_from_dirs(std::slice::from_ref(&skills_dir.to_path_buf())),
        }
    }
```

- [ ] **Step 5: Run the tests to verify they pass**

```bash
cd src-tauri
cargo test 2>&1 | tail -20
```

Expected: PASS, all tests.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/config.rs
git commit -m "feat: whitelist primitives for Claude JSON and Codex TOML configs"
```

---

### Task 4: Codex account discovery

**Files:**
- Create: `src-tauri/src/agents/codex.rs`
- Modify: `src-tauri/src/agents/mod.rs` (declare the module; leave `REGISTRY` alone until Task 8)

**Interfaces:**
- Consumes: `AccountSpec`, `slug` (Task 1); `mcps_from_codex_toml`, `skills_from_dirs` (Task 3).
- Produces: `agents::codex::discover() -> Vec<AccountSpec>` and `agents::codex::load_config(&AccountSpec) -> UserConfig`, both `pub(super)`.

- [ ] **Step 1: Write the failing test**

Create `src-tauri/src/agents/codex.rs` containing only:

```rust
// Codex CLI adapter. Logs live at <codex-dir>/sessions/YYYY/MM/DD/rollout-*.jsonl,
// one session per file. See docs/superpowers/specs/2026-08-12-codex-tracking-design.md.
use super::{AccountSpec, FileState, LogParser};
use crate::config::{self, UserConfig};
use std::collections::HashSet;
use std::path::PathBuf;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dir_is_an_account_only_when_it_has_sessions() {
        let root = std::env::temp_dir().join(format!("ts-cdisc-{}", std::process::id()));
        let real = root.join(".codex");
        let empty = root.join(".codex-empty");
        let _ = std::fs::create_dir_all(real.join("sessions"));
        let _ = std::fs::create_dir_all(&empty);

        let mut out = Vec::new();
        let mut seen = HashSet::new();
        try_add(&mut out, &mut seen, real.clone());
        try_add(&mut out, &mut seen, empty);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].agent, "codex");
        assert_eq!(out[0].log_root, real.join("sessions"));
        assert_eq!(out[0].config_file, real.join("config.toml"));

        // Same dir twice must not produce two accounts (they would share a cache).
        try_add(&mut out, &mut seen, real);
        assert_eq!(out.len(), 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn default_dir_is_labelled_codex_and_siblings_use_their_dir_name() {
        let root = std::env::temp_dir().join(format!("ts-clabel-{}", std::process::id()));
        let _ = std::fs::create_dir_all(root.join(".codex/sessions"));
        let _ = std::fs::create_dir_all(root.join(".codex-work/sessions"));

        assert_eq!(label_for(&root.join(".codex")), "Codex");
        assert_eq!(label_for(&root.join(".codex-work")), "codex-work");

        let _ = std::fs::remove_dir_all(&root);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Add `pub mod codex;` to `agents/mod.rs` (right after `pub mod claude;`), then:

```bash
cd src-tauri
cargo test --lib codex 2>&1 | tail -20
```

Expected: FAIL to compile — `try_add` and `label_for` do not exist.

- [ ] **Step 3: Implement discovery**

Insert above the `mod tests` block in `codex.rs`:

```rust
/// Friendly tab label. The default `~/.codex` is just "Codex"; a sibling config
/// dir shows its own name so two Codex installs are distinguishable. auth.json
/// is deliberately never read — it holds refresh tokens, and the tab is
/// user-renameable anyway.
fn label_for(dir: &std::path::Path) -> String {
    let name = dir
        .file_name()
        .map(|s| s.to_string_lossy().trim_start_matches('.').to_string())
        .unwrap_or_default();
    if name == "codex" {
        "Codex".to_string()
    } else if name.is_empty() {
        "Codex".to_string()
    } else {
        name
    }
}

fn try_add(out: &mut Vec<AccountSpec>, seen: &mut HashSet<PathBuf>, dir: PathBuf) {
    // Only a directory that actually has a sessions/ log dir is an account.
    let log_root = dir.join("sessions");
    if !log_root.is_dir() {
        return;
    }
    // Dedupe by canonical path so ~/.codex, $CODEX_HOME and the sibling scan
    // can't register the same account twice.
    let canon = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
    if !seen.insert(canon) {
        return;
    }
    out.push(AccountSpec {
        id: super::slug(&dir),
        agent: "codex",
        label: label_for(&dir),
        email: String::new(),
        log_root,
        config_file: dir.join("config.toml"),
        // ~/.agents/skills is the shared cross-agent skill root Codex reads.
        skill_dirs: vec![
            dir.join("skills"),
            dirs::home_dir().unwrap_or_default().join(".agents/skills"),
        ],
    });
}

/// All Codex installs on this machine, in a stable order (default first).
pub(super) fn discover() -> Vec<AccountSpec> {
    let mut out: Vec<AccountSpec> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let Some(home) = dirs::home_dir() else {
        return out;
    };

    try_add(&mut out, &mut seen, home.join(".codex"));

    if let Ok(d) = std::env::var("CODEX_HOME") {
        try_add(&mut out, &mut seen, PathBuf::from(d));
    }

    if let Ok(entries) = std::fs::read_dir(&home) {
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with(".codex") && n != ".codex")
                        .unwrap_or(false)
            })
            .collect();
        dirs.sort();
        for d in dirs {
            try_add(&mut out, &mut seen, d);
        }
    }

    out
}

pub(super) fn load_config(a: &AccountSpec) -> UserConfig {
    UserConfig {
        mcp_servers: config::mcps_from_codex_toml(&a.config_file),
        skills: config::skills_from_dirs(&a.skill_dirs),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cd src-tauri
cargo test 2>&1 | tail -20
```

Expected: PASS. Warnings about unused `FileState` / `LogParser` imports are expected until Task 5.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/agents
git commit -m "feat: discover Codex installs and load their MCP/Skill whitelist"
```

---

### Task 5: Codex token accounting

The core of the feature. Tokens come from the difference between consecutive cumulative counters.

**Files:**
- Modify: `src-tauri/src/agents/codex.rs`

**Interfaces:**
- Consumes: `FileState`, `LogParser` (Task 1); `RawEvent` from `store.rs`.
- Produces: `agents::codex::parser() -> Box<dyn LogParser>`, plus internal `CodexState` handling `session_meta`, `turn_context`, `thread_settings_applied` and `token_count`.

- [ ] **Step 1: Write the failing test**

Add these tests inside `codex.rs`'s `mod tests`:

```rust
    fn feed(lines: &[&str]) -> Vec<RawEvent> {
        let p = CodexParser;
        let mut st = p.new_file_state(None);
        lines.iter().filter_map(|l| st.parse_line(l)).collect()
    }

    const META: &str = r#"{"timestamp":"2026-08-12T04:00:00.000Z","type":"session_meta","payload":{"session_id":"s1","cwd":"/w/proj","source":"vscode","git":{"branch":"main"}}}"#;
    const CTX: &str = r#"{"timestamp":"2026-08-12T04:00:01.000Z","type":"turn_context","payload":{"model":"gpt-5.6-sol"}}"#;

    fn tc(ts: &str, input: u64, cached: u64, cw: u64, out: u64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"cache_write_input_tokens":{cw},"output_tokens":{out},"total_tokens":0}}}}}}}}"#
        )
    }

    #[test]
    fn tokens_come_from_the_cumulative_difference() {
        let a = tc("2026-08-12T04:00:02.000Z", 1000, 400, 10, 50);
        let b = tc("2026-08-12T04:00:03.000Z", 3000, 1400, 10, 120);
        let ev = feed(&[META, CTX, &a, &b]);
        assert_eq!(ev.len(), 2);

        // First event: uncached input = 1000 - 400.
        assert_eq!(ev[0].in_tok, 600.0);
        assert_eq!(ev[0].cr, 400.0);
        assert_eq!(ev[0].cc, 10.0);
        assert_eq!(ev[0].out_tok, 50.0);
        assert_eq!(ev[0].model, "gpt-5.6-sol");
        assert_eq!(ev[0].session, "s1");

        // Second event is the delta only: input +2000, cached +1000 → uncached 1000.
        assert_eq!(ev[1].in_tok, 1000.0);
        assert_eq!(ev[1].cr, 1000.0);
        assert_eq!(ev[1].cc, 0.0);
        assert_eq!(ev[1].out_tok, 70.0);
    }

    #[test]
    fn a_repeated_cumulative_snapshot_adds_nothing() {
        // Codex re-emits token_count with an unchanged cumulative; summing
        // last_token_usage would double-count here, diffing must not.
        let a = tc("2026-08-12T04:00:02.000Z", 1000, 400, 0, 50);
        let ev = feed(&[META, CTX, &a, &a, &a]);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].in_tok, 600.0);
    }

    #[test]
    fn a_zero_component_baseline_attributes_nothing() {
        // Observed in real logs: a forked/resumed session opens with a nonzero
        // total_tokens but every component zero. It must contribute no usage.
        let a = tc("2026-08-12T04:00:02.000Z", 0, 0, 0, 0);
        assert!(feed(&[META, CTX, &a]).is_empty());
    }

    #[test]
    fn a_backwards_counter_rebaselines_instead_of_going_negative() {
        let a = tc("2026-08-12T04:00:02.000Z", 5000, 0, 0, 200);
        let b = tc("2026-08-12T04:00:03.000Z", 100, 0, 0, 10);
        let ev = feed(&[META, CTX, &a, &b]);
        assert_eq!(ev.len(), 1);
        assert!(ev.iter().all(|e| e.in_tok >= 0.0 && e.out_tok >= 0.0));
    }

    #[test]
    fn carry_resumes_the_cumulative_across_a_split_read() {
        let p = CodexParser;
        let a = tc("2026-08-12T04:00:02.000Z", 1000, 0, 0, 50);
        let mut st = p.new_file_state(None);
        for l in [META, CTX, a.as_str()] {
            st.parse_line(l);
        }
        let saved = st.carry().expect("codex state must be persistable");

        // Second pass sees only the new line but must diff against 1000/50.
        let b = tc("2026-08-12T04:00:03.000Z", 1500, 0, 0, 70);
        let mut st2 = p.new_file_state(Some(&saved));
        let ev = st2.parse_line(&b).expect("a delta event");
        assert_eq!(ev.in_tok, 500.0);
        assert_eq!(ev.out_tok, 20.0);
        // Session and model must survive too, or the event is unattributable.
        assert_eq!(ev.session, "s1");
        assert_eq!(ev.model, "gpt-5.6-sol");
    }

    #[test]
    fn thread_settings_applied_also_supplies_the_model() {
        let ts = r#"{"timestamp":"2026-08-12T04:00:01.000Z","type":"event_msg","payload":{"type":"thread_settings_applied","thread_settings":{"model":"codex-auto-review"}}}"#;
        let a = tc("2026-08-12T04:00:02.000Z", 100, 0, 0, 10);
        let ev = feed(&[META, ts, &a]);
        assert_eq!(ev[0].model, "codex-auto-review");
    }

    #[test]
    fn an_unknown_model_is_recorded_not_dropped() {
        let a = tc("2026-08-12T04:00:02.000Z", 100, 0, 0, 10);
        let ev = feed(&[META, &a]);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].model, "unknown");
    }
```

Add `use crate::store::RawEvent;` to the test module's imports if not already in scope via `super::*`.

- [ ] **Step 2: Run the test to verify it fails**

```bash
cd src-tauri
cargo test --lib codex 2>&1 | tail -20
```

Expected: FAIL to compile — `CodexParser` does not exist.

- [ ] **Step 3: Implement the parser**

Add to `codex.rs`, above `mod tests`:

```rust
use crate::store::RawEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Cumulative token counters as of one `token_count` event. Codex reports these
/// as running session totals, so usage is the difference between consecutive
/// snapshots — repeated or replayed events then contribute nothing, which
/// summing `last_token_usage` would get wrong.
#[derive(Serialize, Deserialize, Clone, Copy, Default)]
struct Cum {
    input: f64,
    cached: f64,
    cache_write: f64,
    output: f64,
}

/// Parser state persisted between incremental reads of one session file.
#[derive(Serialize, Deserialize, Clone, Default)]
struct Carry {
    session: String,
    model: String,
    cwd: String,
    branch: String,
    sidechain: bool,
    prev: Option<Cum>,
}

pub(super) struct CodexParser;

struct CodexState {
    c: Carry,
}

impl LogParser for CodexParser {
    fn new_file_state(&self, carry: Option<&Value>) -> Box<dyn FileState> {
        let c = carry
            .and_then(|v| serde_json::from_value::<Carry>(v.clone()).ok())
            .unwrap_or_default();
        Box::new(CodexState { c })
    }
}

pub(super) fn parser() -> Box<dyn LogParser> {
    Box::new(CodexParser)
}

fn num(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0)
}

fn ms(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.timestamp_millis())
}

impl CodexState {
    /// A RawEvent pre-filled with this session's attribution, carrying no usage.
    /// Tool/skill events reuse it; `id` stays empty because the byte-offset
    /// manifest already guarantees each line is read exactly once.
    fn base(&self, ts_ms: i64) -> RawEvent {
        RawEvent {
            ts_ms,
            session: self.c.session.clone(),
            model: if self.c.model.is_empty() {
                "unknown".into()
            } else {
                self.c.model.clone()
            },
            in_tok: 0.0,
            cc: 0.0,
            cr: 0.0,
            out_tok: 0.0,
            mcp: Vec::new(),
            skills: Vec::new(),
            id: String::new(),
            source: String::new(),
            cwd: self.c.cwd.clone(),
            branch: self.c.branch.clone(),
            tools: Vec::new(),
            sidechain: self.c.sidechain,
            // Codex has no general tool-error flag; see the spec.
            tool_results: 0,
            tool_errors: 0,
        }
    }

    fn on_session_meta(&mut self, p: &Value) {
        if let Some(s) = p.get("session_id").and_then(|v| v.as_str()) {
            self.c.session = s.to_string();
        }
        if let Some(s) = p.get("cwd").and_then(|v| v.as_str()) {
            self.c.cwd = s.to_string();
        }
        if let Some(s) = p.get("git").and_then(|g| g.get("branch")).and_then(|v| v.as_str()) {
            self.c.branch = s.to_string();
        }
        // A sub-agent thread records its parent under source.subagent; a normal
        // session's source is a plain string ("vscode").
        self.c.sidechain = p
            .get("source")
            .and_then(|v| v.as_object())
            .map(|o| o.contains_key("subagent"))
            .unwrap_or(false);
    }

    fn on_token_count(&mut self, p: &Value, ts_ms: i64) -> Option<RawEvent> {
        let t = p.get("info")?.get("total_token_usage")?;
        let cur = Cum {
            input: num(t, "input_tokens"),
            cached: num(t, "cached_input_tokens"),
            cache_write: num(t, "cache_write_input_tokens"),
            output: num(t, "output_tokens"),
        };
        let prev = self.c.prev.unwrap_or_default();
        self.c.prev = Some(cur);

        // Clamp each delta at zero. The counter is monotonic in practice, but a
        // resumed or forked session can restart it; treat any decrease as a new
        // baseline rather than emitting negative usage.
        let d_in = (cur.input - prev.input).max(0.0);
        let d_cached = (cur.cached - prev.cached).max(0.0);
        let d_cw = (cur.cache_write - prev.cache_write).max(0.0);
        let d_out = (cur.output - prev.output).max(0.0);
        // cached_input_tokens is a subset of input_tokens, not a sibling.
        let uncached = (d_in - d_cached).max(0.0);

        if uncached == 0.0 && d_cached == 0.0 && d_cw == 0.0 && d_out == 0.0 {
            return None;
        }
        let mut e = self.base(ts_ms);
        e.in_tok = uncached;
        e.cr = d_cached;
        e.cc = d_cw;
        e.out_tok = d_out;
        Some(e)
    }
}

impl FileState for CodexState {
    fn parse_line(&mut self, line: &str) -> Option<RawEvent> {
        let v: Value = serde_json::from_str(line).ok()?;
        let ts_ms = ms(v.get("timestamp")?.as_str()?)?;
        let ty = v.get("type")?.as_str()?;
        let p = v.get("payload")?;
        match ty {
            "session_meta" => {
                self.on_session_meta(p);
                None
            }
            // turn_context has no payload.type; the model lives directly on it.
            "turn_context" => {
                if let Some(m) = p.get("model").and_then(|v| v.as_str()) {
                    self.c.model = m.to_string();
                }
                None
            }
            "event_msg" => match p.get("type").and_then(|v| v.as_str())? {
                "thread_settings_applied" => {
                    if let Some(m) = p
                        .get("thread_settings")
                        .and_then(|s| s.get("model"))
                        .and_then(|v| v.as_str())
                    {
                        self.c.model = m.to_string();
                    }
                    None
                }
                "token_count" => self.on_token_count(p, ts_ms),
                _ => None,
            },
            _ => None,
        }
    }

    fn carry(&self) -> Option<Value> {
        serde_json::to_value(&self.c).ok()
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cd src-tauri
cargo test 2>&1 | tail -20
```

Expected: PASS, all tests including the seven new Codex ones.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/agents/codex.rs
git commit -m "feat: parse Codex token usage from cumulative counter deltas"
```

---

### Task 6: Codex tool, MCP and Skill extraction

**Files:**
- Modify: `src-tauri/src/agents/codex.rs`

**Interfaces:**
- Consumes: `CodexState` (Task 5).
- Produces: `skill_names(&str) -> Vec<String>` plus `response_item` / `mcp_tool_call_end` handling; a per-turn skill dedup set on `CodexState`.

- [ ] **Step 1: Write the failing test**

Add to `codex.rs`'s `mod tests`:

```rust
    #[test]
    fn skill_names_finds_every_skill_md_path_in_one_command() {
        // A single exec can open several skills at once.
        let cmd = "sed -n '1,320p' /Users/u/.agents/skills/review-bugbot/SKILL.md && sed -n '1,320p' /Users/u/.agents/skills/review-security/SKILL.md";
        assert_eq!(skill_names(cmd), vec!["review-bugbot", "review-security"]);
    }

    #[test]
    fn skill_names_ignores_non_skill_md_reads() {
        // A reference file under a skill is not a fresh skill invocation, and a
        // nested system path is not a <name>/SKILL.md either.
        assert!(skill_names("cat /r/skills/using-superpowers/references/codex-tools.md").is_empty());
        assert!(skill_names("cat /r/skills/.system/openai-docs/SKILL.md").is_empty());
        assert!(skill_names("ls /r/skills/").is_empty());
    }

    #[test]
    fn a_skill_counts_once_per_turn_but_again_in_the_next_turn() {
        let exec = |c: &str| {
            format!(
                r#"{{"timestamp":"2026-08-12T04:00:05.000Z","type":"response_item","payload":{{"type":"custom_tool_call","name":"exec","input":"{c}"}}}}"#
            )
        };
        let e = exec("cat /r/skills/review/SKILL.md");
        let ev = feed(&[META, CTX, &e, &e, CTX, &e]);
        let skills: Vec<&str> = ev
            .iter()
            .flat_map(|r| r.skills.iter().map(|s| s.as_str()))
            .collect();
        assert_eq!(skills, vec!["review", "review"]);
    }

    #[test]
    fn mcp_tool_call_end_yields_the_server_name() {
        let m = r#"{"timestamp":"2026-08-12T04:00:06.000Z","type":"event_msg","payload":{"type":"mcp_tool_call_end","invocation":{"server":"github","tool":"get_pr_info"}}}"#;
        let ev = feed(&[META, CTX, m]);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].mcp, vec!["github"]);
        assert_eq!(ev[0].tools, vec!["github.get_pr_info"]);
        // A tool call is not usage.
        assert_eq!(ev[0].in_tok, 0.0);
        assert_eq!(ev[0].out_tok, 0.0);
    }

    #[test]
    fn function_calls_are_recorded_as_tools_not_mcp() {
        // These are built-in agent tools, not MCP servers.
        let f = r#"{"timestamp":"2026-08-12T04:00:07.000Z","type":"response_item","payload":{"type":"function_call","name":"spawn_agent","namespace":"collaboration"}}"#;
        let ev = feed(&[META, CTX, f]);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].tools, vec!["spawn_agent"]);
        assert!(ev[0].mcp.is_empty());
    }

    #[test]
    fn a_subagent_session_marks_its_events_as_sidechain() {
        let meta = r#"{"timestamp":"2026-08-12T04:00:00.000Z","type":"session_meta","payload":{"session_id":"s2","cwd":"/w","source":{"subagent":{"other":"guardian"}}}}"#;
        let a = tc("2026-08-12T04:00:02.000Z", 100, 0, 0, 10);
        let ev = feed(&[meta, CTX, &a]);
        assert!(ev[0].sidechain);
    }

    #[test]
    fn session_metadata_reaches_the_events() {
        let a = tc("2026-08-12T04:00:02.000Z", 100, 0, 0, 10);
        let ev = feed(&[META, CTX, &a]);
        assert_eq!(ev[0].cwd, "/w/proj");
        assert_eq!(ev[0].branch, "main");
        assert_eq!(ev[0].tool_results, 0);
        assert_eq!(ev[0].tool_errors, 0);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cd src-tauri
cargo test --lib codex 2>&1 | tail -20
```

Expected: FAIL to compile — `skill_names` does not exist.

- [ ] **Step 3: Implement extraction**

Add the free function to `codex.rs`:

```rust
/// Every `skills/<name>/SKILL.md` path in a shell command, in order.
///
/// Codex has no skill tool call: a skill is invoked by reading its SKILL.md, so
/// that read is the signal. Requiring the exact `<name>/SKILL.md` tail keeps
/// out reference files under a skill and nested system paths
/// (`skills/.system/openai-docs/SKILL.md`), and one command can open several.
fn skill_names(cmd: &str) -> Vec<String> {
    const MARK: &str = "skills/";
    let mut out = Vec::new();
    let mut rest = cmd;
    while let Some(i) = rest.find(MARK) {
        rest = &rest[i + MARK.len()..];
        let Some(name) = rest.split('/').next() else {
            continue;
        };
        if !name.is_empty() && rest[name.len()..].starts_with("/SKILL.md") {
            let n = name.to_string();
            if !out.contains(&n) {
                out.push(n);
            }
        }
    }
    out
}
```

Add the per-turn dedup set to the state struct and its constructor:

```rust
struct CodexState {
    c: Carry,
    /// Skills already counted in the current turn, so re-reading a SKILL.md
    /// mid-turn doesn't inflate the count. Deliberately not carried across an
    /// incremental read: turn boundaries reset it anyway.
    turn_skills: HashSet<String>,
}
```

In `new_file_state`, build it as `Box::new(CodexState { c, turn_skills: HashSet::new() })`.

Add the handler:

```rust
impl CodexState {
    fn on_response_item(&mut self, p: &Value, ts_ms: i64) -> Option<RawEvent> {
        let kind = p.get("type").and_then(|v| v.as_str())?;
        if kind != "function_call" && kind != "custom_tool_call" {
            return None;
        }
        let name = p.get("name").and_then(|v| v.as_str())?;
        let mut e = self.base(ts_ms);
        e.tools.push(name.to_string());
        if let Some(input) = p.get("input").and_then(|v| v.as_str()) {
            for s in skill_names(input) {
                if self.turn_skills.insert(s.clone()) {
                    e.skills.push(s);
                }
            }
        }
        Some(e)
    }

    fn on_mcp_call(&mut self, p: &Value, ts_ms: i64) -> Option<RawEvent> {
        let inv = p.get("invocation")?;
        let server = inv.get("server").and_then(|v| v.as_str())?;
        let mut e = self.base(ts_ms);
        e.mcp.push(server.to_string());
        if let Some(tool) = inv.get("tool").and_then(|v| v.as_str()) {
            e.tools.push(format!("{server}.{tool}"));
        }
        Some(e)
    }
}
```

Wire them into `parse_line`. In the `"turn_context"` arm, add `self.turn_skills.clear();` after setting the model. In the `"event_msg"` match, add arms:

```rust
                "task_started" => {
                    self.turn_skills.clear();
                    None
                }
                "mcp_tool_call_end" => self.on_mcp_call(p, ts_ms),
```

and add a top-level arm alongside `"event_msg"`:

```rust
            "response_item" => self.on_response_item(p, ts_ms),
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cd src-tauri
cargo test 2>&1 | tail -20
```

Expected: PASS, all tests.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/agents/codex.rs
git commit -m "feat: extract Codex tool, MCP and Skill calls from session logs"
```

---

### Task 7: Register Codex and surface the agent in the API

**Files:**
- Modify: `src-tauri/src/agents/mod.rs`, `src-tauri/src/agents/codex.rs`, `src-tauri/src/model.rs`, `src-tauri/src/parser.rs`

**Interfaces:**
- Consumes: everything from Tasks 4–6.
- Produces: `agents::codex::DESCRIPTOR`; `AccountData.agent: String` serialized as `agent`; `vendor_of("codex-auto-review") == "OpenAI"`.

- [ ] **Step 1: Write the failing test**

Add to `parser.rs`'s test module (create one at the bottom of the file if absent):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_models_are_attributed_to_openai() {
        assert_eq!(vendor_of("gpt-5.6-sol"), "OpenAI");
        // Has no "gpt" in the name, so it needs its own rule or it lands in "Other".
        assert_eq!(vendor_of("codex-auto-review"), "OpenAI");
        // Unchanged for the models already handled.
        assert_eq!(vendor_of("claude-opus-5"), "Anthropic");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cd src-tauri
cargo test --lib codex_models 2>&1 | tail -20
```

Expected: FAIL — `assertion failed: "Other" == "OpenAI"`.

- [ ] **Step 3: Implement the registration and the agent field**

In `parser.rs`, extend `vendor_of`:

```rust
    if m.contains("claude") {
        "Anthropic"
    } else if m.contains("gpt") || m.contains("o1") || m.contains("o3") || m.contains("codex") {
        "OpenAI"
```

In `codex.rs`, add the descriptor:

```rust
pub const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "codex",
    display: "Codex",
    discover,
    load_config,
    parser,
};
```

and add `use super::AgentDescriptor;` to its imports.

In `agents/mod.rs`, register it:

```rust
static REGISTRY: &[AgentDescriptor] = &[claude::DESCRIPTOR, codex::DESCRIPTOR];
```

In `model.rs`, add the field to `AccountData`:

```rust
pub struct AccountData {
    pub id: String,    // stable key (slug of the config dir), also the tab key
    pub label: String, // friendly name (org / display name / email / dir)
    pub email: String, // account email if known (may be empty)
    pub agent: String, // owning CLI ("claude" / "codex") — drives the tab badge
    pub dash: Dashboard,
}
```

In `parser.rs`'s `build_workspace`, populate it:

```rust
        accounts.push(AccountData {
            id: a.id,
            label: a.label,
            email: a.email,
            agent: a.agent.to_string(),
            dash,
        });
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cd src-tauri
cargo test 2>&1 | tail -20
cargo check --message-format short 2>&1 | tail -5
```

Expected: PASS, 0 errors.

- [ ] **Step 5: Verify against real Codex data**

```bash
cd src-tauri
cargo run --example dump > /tmp/tokenscope-codex.json
python3 -c "
import json
d = json.load(open('/tmp/tokenscope-codex.json'))
ms = d['month']['models']
for m in ms: print(f\"{m['name']:<24} {m['vendor']:<10} {m['tokens']:>10.2f}M  \${m['cost']:.2f}  priced={m['priced']}\")
"
```

Expected: OpenAI models (`gpt-5.6-sol`, `codex-auto-review`) now appear next to the Anthropic ones. If none appear, check that `~/.codex/sessions` exists and re-run. Cross-check one session against its own log:

```bash
F=$(find ~/.codex/sessions -name '*.jsonl' -size +100k | head -1)
jq -s '[.[] | select(.payload.type=="token_count")] | last | .payload.info.total_token_usage' "$F"
```

The `input_tokens + output_tokens` there should be the order of magnitude that session contributes. An exact match is not expected (the dashboard sums many sessions and prunes to 210 days), but a wild mismatch means the delta logic is wrong.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src
git commit -m "feat: register the Codex agent and expose it in the workspace API"
```

---

### Task 8: Frontend — agent badge on account tabs

**Files:**
- Modify: `src/data.ts`, `src/App.tsx`

**Interfaces:**
- Consumes: `AccountData.agent` from Task 7.
- Produces: `AgentBadge` component; the tab type widens to `{ id: string; label: string; agent: string }`.

- [ ] **Step 1: Add the type and the dev fallback**

In `src/data.ts`:

```ts
// One tracked account (= one agent CLI config dir) and its dashboard.
export interface AccountData { id: string; label: string; email: string; agent: string; dash: Dashboard }
```

and in the browser fallback inside `fetchWorkspace`:

```ts
  return { accounts: [{ id: "dev", label: "Dev", email: "", agent: "claude", dash }], all: dash, todayTokens: dash.todayTokens };
```

- [ ] **Step 2: Verify the typecheck fails where the badge is needed**

```bash
npx tsc --noEmit 2>&1 | head -20
```

Expected: errors where `AccountTabs` builds its `tabs` array without `agent` (it currently maps to `{ id, label }`). This is the list of call sites to update.

- [ ] **Step 3: Thread `agent` into the tabs and render a badge**

In `App.tsx`, widen the tab type everywhere it appears (`AccountTabs` props and `Panel` props) from `{ id: string; label: string }[]` to `{ id: string; label: string; agent: string }[]`, and where the tabs array is built from the workspace, carry `agent: a.agent` through. The aggregate "All" tab uses `agent: "all"`.

Add the badge component next to `Label`:

```tsx
// Tiny per-agent mark on each tab, so a Claude and a Codex account are
// distinguishable without spending a second row of navigation on it.
const AgentBadge = ({ t, agent }: { t: Theme; agent: string }) => {
  if (agent !== "claude" && agent !== "codex") return null;
  return (
    <span aria-hidden="true" style={{
      font: `600 8px ${t.mono}`, color: t.faint, marginRight: 4, opacity: 0.85,
    }}>{agent === "codex" ? "▲" : "◆"}</span>
  );
};
```

and render it inside the tab button, before the label text:

```tsx
        <AgentBadge t={t} agent={tab.agent} />
```

- [ ] **Step 4: Confirm the MCP/Skill sections need no change**

Read `src/App.tsx:585-610`. Both sections are already gated on `M.servers > 0` and `M.skills > 0` — the size of the account's *whitelist*, not of the call list. That is exactly the behaviour Codex needs:

- A Codex account with no `[mcp_servers.*]` and no skill dirs hides both sections automatically.
- One with servers configured but no user-MCP calls (the common case, since the built-in `codex_apps` connector is correctly filtered out) shows the existing "No MCP calls in this period" line, which is accurate.

**Make no edit here.** Re-gating on `P.mcp.length` would suppress that message for Claude users too, which is a regression. This step exists so you verify the assumption rather than change code on autopilot.

- [ ] **Step 5: Verify**

```bash
npx tsc --noEmit && echo "TYPECHECK OK"
```

Expected: `TYPECHECK OK`.

```bash
cd src-tauri && cargo run --example dump > ../public/dev-dashboard.json && cd ..
pnpm dev
```

Open http://localhost:1420. Expected: the panel renders; with the single-account dev snapshot no badge row appears (tabs only show when `tabs.length > 1`). Stop the dev server when done.

- [ ] **Step 6: Commit**

```bash
git add src/data.ts src/App.tsx
git commit -m "feat: badge account tabs by agent"
```

---

### Task 9: End-to-end verification and documentation

**Files:**
- Modify: `README.md`, `README-zh.md`

- [ ] **Step 1: Run the full check**

```bash
cd src-tauri && cargo test 2>&1 | tail -20 && cargo check --message-format short 2>&1 | tail -5
cd .. && npx tsc --noEmit && echo "TYPECHECK OK"
```

Expected: all tests pass, 0 compile errors, `TYPECHECK OK`.

- [ ] **Step 2: Verify the app end to end**

```bash
pnpm tauri dev
```

Expected, in the menu-bar panel:
- A `Codex` tab sits beside the Claude account tab(s), marked `▲`; Claude tabs are marked `◆`.
- The Codex tab shows non-zero tokens and a cost for `gpt-5.6-sol`, and `codex-auto-review` listed without a cost (unpriced).
- The Codex tab's MCP section reads "No MCP calls in this period" — the only servers in the logs are the built-in `codex_apps` connector, correctly filtered out as not user-installed, while `M.servers` still counts the `[mcp_servers.*]` entries in `config.toml`.
- The Codex tab's Skill section lists skills whose names match a directory in `~/.codex/skills/` or `~/.agents/skills/` (e.g. `review-security`, `review-bugbot`, `payment-integration`).
- The `All` tab's total is the sum of the per-agent tabs.
- Writing to a Codex session refreshes the panel within a few seconds (watcher covers `sessions/`).

Record anything that does not match; do not proceed past a mismatch in the token totals.

- [ ] **Step 3: Update the README data-source tables**

In `README.md`, extend the "Data sources" table with the Codex rows:

```markdown
| Codex session logs (tokens / model / tool calls) | `~/.codex/sessions/**/*.jsonl` |
| Codex MCP whitelist | `~/.codex/config.toml` → `[mcp_servers.*]` |
| Codex Skill whitelist | `~/.codex/skills/` and `~/.agents/skills/` |
```

Update the opening description from "your Claude CLI daily token usage" to "your Claude Code and OpenAI Codex daily token usage", and add to the Structure block:

```
  agents/mod.rs     agent adapter registry (where logs live, how to parse them)
  agents/claude.rs  Claude Code discovery + log parsing
  agents/codex.rs   Codex discovery + log parsing (cumulative token deltas)
```

Mirror all of the above in `README-zh.md`.

- [ ] **Step 4: Commit**

```bash
git add README.md README-zh.md
git commit -m "docs: document Codex as a tracked agent"
```

---

## Notes for the implementer

- **The one thing most likely to go wrong** is Codex token attribution. If the Codex tab shows numbers that look ~2× too big, the cumulative baseline is being lost between incremental reads — check that `FileEntry.carry` is written on every pass, including passes that emit no events.
- `cargo test` output is noisy; `| tail -20` keeps the summary visible.
- The `dump` example prints the **aggregate** dashboard, not per-account. To inspect one account, add a temporary `build_period("<account-id>", "Month", Local::now())` call rather than changing `dump`.
- Do not "fix" the fact that Codex sessions carry no reliability data. That is a deliberate decision recorded in the spec.
