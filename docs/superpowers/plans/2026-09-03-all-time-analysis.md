# All-Time Analysis Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an **All time** page to the Tokenscope panel, backed by a durable per-day rollup archive that outlives the 210-day raw-event prune.

**Architecture:** A new `rollup.rs` persists one small row per calendar day per account, in its own never-pruned cache file. Every dashboard build folds the live (un-pruned) events into that archive, overwriting the rows for days the raw store still fully covers. Because the archive is a map keyed by ISO date, "archive rows for old days + live rows for recent days" is structural rather than a merge step — a day cannot be counted twice. Names are archived unfiltered and per-model tokens are archived raw, so the MCP/skill whitelist and the price table are applied when the archive is *read*, keeping both retroactive forever.

**Tech Stack:** Rust (chrono, serde, serde_json) + React 18 / TypeScript / Vite, Tauri 2.

**Spec:** `docs/superpowers/specs/2026-09-03-all-time-analysis-design.md`

## Global Constraints

- Rust tests run with `cargo test --lib` from `src-tauri/`. **Baseline before this plan: 99 passed, 2 ignored, exit 0.** Every task must leave it green.
- There is no frontend test infrastructure in this repo. Frontend tasks are verified by `pnpm build` (typecheck) plus running the app. Do not add a test framework.
- Cache files live in `dirs::cache_dir()?.join("tokenscope")`. The rollup file is `rollup-<account-id>.json`, a **separate file** from `store-<id>.json`.
- All persisted writes go through a temp-file-then-rename, matching `store.rs::write_atomic`.
- Token figures crossing the Rust/TS boundary are **M tokens** (raw ÷ 1e6), rounded to 2 dp via the existing `r2` helper. Cost is USD, 2 dp.
- Serde field renames are camelCase to match the existing `model.rs` style (`#[serde(rename = "...")]`).
- Do not change the 210-day raw retention.
- Match surrounding comment density: these files carry "why", not "what". Explain a non-obvious invariant; never narrate the code.

---

### Task 1: The archive file — types, persistence, versioning

**Files:**
- Create: `src-tauri/src/rollup.rs`
- Modify: `src-tauri/src/lib.rs:1-7` (module list)
- Test: `src-tauri/src/rollup.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: `pub struct TokBits { pub input: f64, pub cc: f64, pub cr: f64, pub out: f64, pub requests: u64 }`; `pub struct DayRow { .. }` (all fields `pub`); `pub struct Archive { pub days: BTreeMap<String, DayRow> }`; `Archive::load(id: &str) -> Archive`; `Archive::save(&self, id: &str)`; `pub const ROLLUP_VERSION: u32`.

- [ ] **Step 1: Write the failing tests**

Append to `src-tauri/src/rollup.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn row(date: &str, out: f64) -> DayRow {
        let mut r = DayRow::new(date);
        r.models.insert(
            "claude-opus-5".to_string(),
            TokBits { input: 10.0, cc: 0.0, cr: 0.0, out, requests: 1 },
        );
        r
    }

    #[test]
    fn an_archive_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("ts-roll-rt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);

        let mut a = Archive::default();
        a.days.insert("2026-01-05".to_string(), row("2026-01-05", 7.0));
        a.save_to(&dir, "acct");

        let back = Archive::load_from(&dir, "acct");
        assert_eq!(back.days.len(), 1);
        assert_eq!(back.days["2026-01-05"].models["claude-opus-5"].out, 7.0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_archive_from_an_older_version_is_discarded_whole() {
        // A format change must lose history rather than misread it: up to 210
        // days rebuild themselves from the raw store on the next build.
        let dir = std::env::temp_dir().join(format!("ts-roll-ver-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        fs::write(
            dir.join("rollup-acct.json"),
            serde_json::json!({
                "version": ROLLUP_VERSION - 1,
                "days": { "2026-01-05": { "date": "2026-01-05" } }
            })
            .to_string(),
        )
        .unwrap();

        assert!(Archive::load_from(&dir, "acct").days.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_archive_is_discarded_whole() {
        let dir = std::env::temp_dir().join(format!("ts-roll-trunc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);

        let mut a = Archive::default();
        a.days.insert("2026-01-05".to_string(), row("2026-01-05", 7.0));
        a.save_to(&dir, "acct");

        let p = dir.join("rollup-acct.json");
        let whole = fs::read_to_string(&p).unwrap();
        fs::write(&p, &whole[..whole.len() / 2]).unwrap();
        assert!(Archive::load_from(&dir, "acct").days.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo test --lib rollup`
Expected: FAIL — `file not found for module `rollup`` (the module isn't registered yet).

- [ ] **Step 3: Write the minimal implementation**

Create `src-tauri/src/rollup.rs` **above** the test module written in Step 1:

```rust
// Durable per-day rollup archive.
//
// The raw event store is pruned to 210 days (see parser.rs), and pruned events
// never come back — an old log already read to EOF is never re-read. This file
// is what makes "all time" mean all time: one small, self-describing row per
// calendar day, in its own never-pruned document.
//
// Two things are deliberately NOT baked into a row, because the app applies
// them at read time and they must stay retroactive: MCP/skill names are stored
// unfiltered (the whitelist is applied when the row is read), and per-model
// tokens are stored raw (prices are applied when the row is read).
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;

// Bump when a row's meaning changes. A mismatch discards the archive whole:
// up to 210 days rebuild from the raw store, and older history is lost, which
// is strictly better than silently misreading it.
pub const ROLLUP_VERSION: u32 = 1;

/// Raw token components for one model on one day, plus its request count.
/// Kept raw so cost is re-derived from the *current* price table on read.
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct TokBits {
    pub input: f64,
    pub cc: f64,
    pub cr: f64,
    pub out: f64,
    pub requests: u64,
}

/// One calendar day of usage for one account.
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct DayRow {
    pub date: String, // ISO yyyy-mm-dd
    /// RAW model id (the price-lookup key), not the normalized display name.
    #[serde(default)]
    pub models: HashMap<String, TokBits>,
    /// (M tokens, USD). These costs are frozen at archive time — deriving them
    /// on read would need a project×model cross product per day.
    #[serde(default)]
    pub projects: HashMap<String, (f64, f64)>,
    #[serde(default)]
    pub branches: HashMap<String, (f64, f64)>,
    #[serde(default)]
    pub accounts: HashMap<String, (f64, f64)>,
    /// Unfiltered names — the whitelist is applied on read.
    #[serde(default)]
    pub tools: HashMap<String, u64>,
    #[serde(default)]
    pub mcp: HashMap<String, u64>,
    #[serde(default)]
    pub skills: HashMap<String, u64>,
    /// Hour-of-day token histogram, M tokens.
    #[serde(default)]
    pub hourly: Vec<f64>,
    /// Sessions distinct *within this day*. Summing across days double-counts a
    /// session that spans local midnight; the alternative is archiving an
    /// unbounded id set. Bounded at one per crossing, and accepted.
    #[serde(default)]
    pub sessions: u64,
    /// Raw tokens spent inside subagents (isSidechain).
    #[serde(default)]
    pub subagent: f64,
    #[serde(default)]
    pub tool_results: u64,
    #[serde(default)]
    pub tool_errors: u64,
}

impl DayRow {
    pub fn new(date: &str) -> Self {
        DayRow {
            date: date.to_string(),
            hourly: vec![0.0; 24],
            ..Default::default()
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
pub struct Archive {
    /// ISO date -> row. A date-keyed map is what makes the archive/live split
    /// structural: absorbing a live day overwrites its row instead of adding a
    /// second copy, so no day can ever be counted twice.
    pub days: BTreeMap<String, DayRow>,
}

#[derive(Serialize)]
struct DocRef<'a> {
    version: u32,
    days: &'a BTreeMap<String, DayRow>,
}

#[derive(Deserialize)]
struct Doc {
    version: u32,
    days: BTreeMap<String, DayRow>,
}

fn write_atomic(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)
}

fn cache_dir() -> Option<PathBuf> {
    let d = dirs::cache_dir()?.join("tokenscope");
    let _ = fs::create_dir_all(&d);
    Some(d)
}

impl Archive {
    pub fn load(id: &str) -> Self {
        match cache_dir() {
            Some(d) => Self::load_from(&d, id),
            None => Archive::default(),
        }
    }

    /// `load`, against an explicit cache directory (so it is testable).
    fn load_from(dir: &std::path::Path, id: &str) -> Self {
        fs::read_to_string(dir.join(format!("rollup-{id}.json")))
            .ok()
            .and_then(|t| serde_json::from_str::<Doc>(&t).ok())
            .filter(|d| d.version == ROLLUP_VERSION)
            .map(|d| Archive { days: d.days })
            .unwrap_or_default()
    }

    pub fn save(&self, id: &str) {
        if let Some(d) = cache_dir() {
            self.save_to(&d, id);
        }
    }

    /// `save`, against an explicit cache directory (so it is testable).
    fn save_to(&self, dir: &std::path::Path, id: &str) {
        let doc = DocRef {
            version: ROLLUP_VERSION,
            days: &self.days,
        };
        if let Ok(t) = serde_json::to_string(&doc) {
            let _ = write_atomic(&dir.join(format!("rollup-{id}.json")), t.as_bytes());
        }
    }
}
```

Then register the module. In `src-tauri/src/lib.rs`, the module list at lines 1-7 is alphabetical; insert `mod rollup;` between `mod quota;` and `mod store;`:

```rust
mod quota;
mod rollup;
mod store;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo test --lib rollup`
Expected: PASS, 3 tests. Then `cargo test --lib` → 102 passed, 2 ignored.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/rollup.rs src-tauri/src/lib.rs
git commit -m "feat: add a versioned, atomically-written daily rollup archive"
```

---

### Task 2: Build day rows from raw events

**Files:**
- Modify: `src-tauri/src/rollup.rs`
- Test: `src-tauri/src/rollup.rs` (inline tests)

**Interfaces:**
- Consumes: `DayRow`, `TokBits` (Task 1); `crate::store::RawEvent`; `crate::pricing::Pricing`.
- Produces: `pub fn rows_from_events(events: &[RawEvent], project_of: &mut dyn FnMut(&str) -> String, account_label: &str, pricing: &Pricing) -> BTreeMap<String, DayRow>`.

The resolver is `FnMut`, not `Fn`, deliberately: the caller's real resolver memoizes into a `HashMap` and so captures it mutably. A `&dyn Fn` parameter would not accept it.

Rows are built from `RawEvent`, **not** from `parser::Event`, for two reasons that a reviewer should be able to check: `Event.model` is already normalized (the archive needs the raw id for price lookup) and `Event.mcp`/`Event.skills` are already whitelist-filtered (the archive needs them unfiltered).

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src-tauri/src/rollup.rs`:

```rust
    use crate::store::RawEvent;

    fn raw(ts_ms: i64, model: &str, session: &str) -> RawEvent {
        RawEvent {
            ts_ms,
            session: session.into(),
            model: model.into(),
            in_tok: 100.0,
            cc: 0.0,
            cr: 0.0,
            out_tok: 10.0,
            mcp: vec!["mcp__github".into()],
            skills: vec!["gstack:review".into()],
            id: String::new(),
            source: "/logs/a.jsonl".into(),
            cwd: "/w/repo".into(),
            branch: "main".into(),
            tools: vec!["Read".into(), "mcp__github".into()],
            sidechain: false,
            tool_results: 2,
            tool_errors: 1,
        }
    }

    // 2026-01-05T09:00:00Z as ms; the local hour is asserted from the event
    // itself so the test is timezone-independent.
    const TS: i64 = 1_767_603_600_000;

    #[test]
    fn a_row_carries_unfiltered_names_and_raw_model_tokens() {
        let p = Pricing::empty();
        let mut proj = |_: &str| "repo".to_string();
        let rows = rows_from_events(&[raw(TS, "claude-opus-5-20260101", "s1")], &mut proj, "Work", &p);
        let (_, r) = rows.iter().next().unwrap();

        // Raw id, so the price table can be applied on read.
        let bits = &r.models["claude-opus-5-20260101"];
        assert_eq!(bits.input, 100.0);
        assert_eq!(bits.out, 10.0);
        assert_eq!(bits.requests, 1);
        // Unfiltered: no whitelist has been applied.
        assert_eq!(r.mcp["mcp__github"], 1);
        assert_eq!(r.skills["gstack:review"], 1);
        // Tools keep the mcp__ entry too; parser.rs drops it on read.
        assert_eq!(r.tools["Read"], 1);
        assert_eq!(r.tools["mcp__github"], 1);
        assert_eq!(r.sessions, 1);
        assert_eq!(r.tool_results, 2);
        assert_eq!(r.tool_errors, 1);
    }

    #[test]
    fn an_event_with_no_model_counts_its_tools_but_not_a_request_or_session() {
        // An empty model marks a record that is not an LLM request (Claude's
        // slash-command lines, Codex's tool records). Counting one as a request
        // or a session fabricates activity — the same guard Agg::add applies.
        let p = Pricing::empty();
        let mut proj = |_: &str| "repo".to_string();
        let rows = rows_from_events(&[raw(TS, "", "s1")], &mut proj, "Work", &p);
        let (_, r) = rows.iter().next().unwrap();

        assert!(r.models.is_empty());
        assert_eq!(r.sessions, 0);
        assert_eq!(r.tools["Read"], 1);
        assert_eq!(r.mcp["mcp__github"], 1);
    }

    #[test]
    fn events_group_by_local_calendar_day_and_hour() {
        let p = Pricing::empty();
        let day_ms = 86_400_000;
        let mut proj = |_: &str| "repo".to_string();
        let rows = rows_from_events(
            &[raw(TS, "m", "s1"), raw(TS + day_ms, "m", "s2")],
            &mut proj,
            "Work",
            &p,
        );
        assert_eq!(rows.len(), 2);
        // Each row books its tokens into exactly one hour bucket.
        for r in rows.values() {
            assert_eq!(r.hourly.len(), 24);
            let total: f64 = r.hourly.iter().sum();
            assert!((total - 110.0 / 1e6).abs() < 1e-12);
        }
    }

    #[test]
    fn a_session_spanning_two_days_counts_once_in_each() {
        let p = Pricing::empty();
        let day_ms = 86_400_000;
        let mut proj = |_: &str| "repo".to_string();
        let rows = rows_from_events(
            &[raw(TS, "m", "s1"), raw(TS + day_ms, "m", "s1")],
            &mut proj,
            "Work",
            &p,
        );
        assert_eq!(rows.len(), 2);
        for r in rows.values() {
            assert_eq!(r.sessions, 1);
        }
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo test --lib rollup`
Expected: FAIL — `cannot find function `rows_from_events`` and `no function or associated item named `empty` found for struct `Pricing``.

- [ ] **Step 3: Write the minimal implementation**

First add a test-only constructor to `src-tauri/src/pricing.rs`. Place it immediately before `pub fn cost(` (around line 372), inside the same `impl Pricing` block:

```rust
    /// An empty table: every lookup misses. Test-only — production code always
    /// goes through `load`/`shared`, which fall back to a built-in snapshot.
    #[cfg(test)]
    pub fn empty() -> Self {
        Self::default()
    }
```

`Pricing` does **not** currently derive `Default` (it is a bare `struct Pricing { exact, norm }` at `pricing.rs:41`, holding only `HashMap`s), so add the derive:

```rust
#[derive(Default)]
pub struct Pricing {
```

Then add to `src-tauri/src/rollup.rs`, after the `impl Archive` block:

```rust
use crate::pricing::Pricing;
use crate::store::RawEvent;
use chrono::{DateTime, Local, Timelike};
use std::collections::HashSet;

/// Fold raw events into one row per local calendar day.
///
/// Built from `RawEvent` rather than `parser::Event` on purpose: `Event.model`
/// is already normalized (the archive needs the raw id as a price key) and
/// `Event.mcp`/`skills` are already whitelist-filtered (the archive needs them
/// unfiltered). `project_of` resolves a cwd to its project name — the caller
/// passes its memoized resolver so the filesystem walk isn't repeated per event.
pub fn rows_from_events(
    events: &[RawEvent],
    project_of: &mut dyn FnMut(&str) -> String,
    account_label: &str,
    pricing: &Pricing,
) -> BTreeMap<String, DayRow> {
    let mut rows: BTreeMap<String, DayRow> = BTreeMap::new();
    // Session ids seen per day, collapsed into a count once we're done.
    let mut seen: BTreeMap<String, HashSet<String>> = BTreeMap::new();

    for e in events {
        let ts: DateTime<Local> = DateTime::from_timestamp_millis(e.ts_ms)
            .unwrap_or_default()
            .with_timezone(&Local);
        let date = ts.date_naive().format("%Y-%m-%d").to_string();
        let row = rows
            .entry(date.clone())
            .or_insert_with(|| DayRow::new(&date));

        let tok = e.in_tok + e.cc + e.cr + e.out_tok;
        let cost = pricing
            .cost(&e.model, e.in_tok, e.out_tok, e.cc, e.cr)
            .unwrap_or(0.0);

        // Tools/MCP/Skills count on every event; models, requests and sessions
        // skip model-less records. Mirrors Agg::add exactly.
        for t in &e.tools {
            *row.tools.entry(t.clone()).or_default() += 1;
        }
        for s in &e.mcp {
            *row.mcp.entry(s.clone()).or_default() += 1;
        }
        for s in &e.skills {
            *row.skills.entry(s.clone()).or_default() += 1;
        }
        row.tool_results += e.tool_results as u64;
        row.tool_errors += e.tool_errors as u64;
        row.hourly[ts.hour() as usize] += tok / 1e6;

        if e.model.is_empty() {
            continue;
        }
        if !e.session.is_empty() {
            seen.entry(date.clone()).or_default().insert(e.session.clone());
        }
        let bits = row.models.entry(e.model.clone()).or_default();
        bits.input += e.in_tok;
        bits.cc += e.cc;
        bits.cr += e.cr;
        bits.out += e.out_tok;
        bits.requests += 1;

        if e.sidechain {
            row.subagent += tok;
        }
        let project = project_of(&e.cwd);
        if !project.is_empty() {
            let p = row.projects.entry(project).or_default();
            p.0 += tok / 1e6;
            p.1 += cost;
        }
        if !e.branch.is_empty() {
            let b = row.branches.entry(e.branch.clone()).or_default();
            b.0 += tok / 1e6;
            b.1 += cost;
        }
        if !account_label.is_empty() {
            let a = row.accounts.entry(account_label.to_string()).or_default();
            a.0 += tok / 1e6;
            a.1 += cost;
        }
    }

    for (date, ids) in seen {
        if let Some(r) = rows.get_mut(&date) {
            r.sessions = ids.len() as u64;
        }
    }
    rows
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo test --lib rollup`
Expected: PASS, 7 tests. Then `cargo test --lib` → 106 passed, 2 ignored.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/rollup.rs src-tauri/src/pricing.rs
git commit -m "feat: fold raw events into per-day rollup rows"
```

---

### Task 3: Absorb live rows without clobbering complete history

**Files:**
- Modify: `src-tauri/src/rollup.rs`
- Test: `src-tauri/src/rollup.rs` (inline tests)

**Interfaces:**
- Consumes: `Archive`, `DayRow`, `rows_from_events` (Tasks 1-2).
- Produces: `pub fn absorb(&mut self, live: BTreeMap<String, DayRow>, cutoff_date: NaiveDate)` on `Archive`.

This is the task that carries the plan's central invariant, and it has one sharp edge. `Store::prune_before` cuts on a *timestamp*, not a date, so the day containing the cutoff instant is only **partially** present in the raw store. Rewriting that day's row from live events would overwrite a complete row with a partial one, and it would do so on every single build. So:

- days **strictly after** `cutoff_date` are fully present → always rewrite;
- the boundary day and older are read from the archive, **except** when the archive has no row for that day at all (first run), where a partial row beats a missing one.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src-tauri/src/rollup.rs`:

```rust
    use chrono::NaiveDate;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn live(dates: &[(&str, f64)]) -> BTreeMap<String, DayRow> {
        dates
            .iter()
            .map(|(s, out)| (s.to_string(), row(s, *out)))
            .collect()
    }

    #[test]
    fn absorbing_twice_rewrites_a_day_instead_of_adding_a_second_copy() {
        let mut a = Archive::default();
        a.absorb(live(&[("2026-03-10", 5.0)]), d("2026-01-01"));
        a.absorb(live(&[("2026-03-10", 5.0)]), d("2026-01-01"));

        assert_eq!(a.days.len(), 1);
        assert_eq!(a.days["2026-03-10"].models["claude-opus-5"].out, 5.0);
    }

    #[test]
    fn a_day_inside_the_window_takes_the_newer_value() {
        // Prices or the whitelist may have changed; a day the raw store still
        // fully covers must be re-derived, not preserved.
        let mut a = Archive::default();
        a.absorb(live(&[("2026-03-10", 5.0)]), d("2026-01-01"));
        a.absorb(live(&[("2026-03-10", 9.0)]), d("2026-01-01"));

        assert_eq!(a.days["2026-03-10"].models["claude-opus-5"].out, 9.0);
    }

    #[test]
    fn the_partial_boundary_day_never_overwrites_a_complete_row() {
        // The prune cuts on a timestamp, so the cutoff's own day is only partly
        // in the raw store. Yesterday's build archived it complete; today's
        // partial view must not replace that.
        let mut a = Archive::default();
        a.absorb(live(&[("2026-03-10", 9.0)]), d("2026-03-09")); // complete
        a.absorb(live(&[("2026-03-10", 2.0)]), d("2026-03-10")); // now partial

        assert_eq!(a.days["2026-03-10"].models["claude-opus-5"].out, 9.0);
    }

    #[test]
    fn a_boundary_day_with_no_row_yet_is_archived_partial() {
        // First run: a partial row beats losing the day entirely.
        let mut a = Archive::default();
        a.absorb(live(&[("2026-03-10", 2.0)]), d("2026-03-10"));

        assert_eq!(a.days["2026-03-10"].models["claude-opus-5"].out, 2.0);
    }

    #[test]
    fn a_day_older_than_the_boundary_is_never_touched_by_live_rows() {
        let mut a = Archive::default();
        a.days.insert("2026-01-05".to_string(), row("2026-01-05", 42.0));
        a.absorb(live(&[("2026-01-05", 1.0), ("2026-03-10", 5.0)]), d("2026-03-01"));

        assert_eq!(a.days["2026-01-05"].models["claude-opus-5"].out, 42.0);
        assert_eq!(a.days["2026-03-10"].models["claude-opus-5"].out, 5.0);
        assert_eq!(a.days.len(), 2);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo test --lib rollup`
Expected: FAIL — `no method named `absorb` found for struct `Archive``.

- [ ] **Step 3: Write the minimal implementation**

Add to the `impl Archive` block in `src-tauri/src/rollup.rs`:

```rust
    /// Fold this build's live day rows into the archive.
    ///
    /// `cutoff_date` is the calendar date of the raw store's prune cutoff. The
    /// prune cuts on a *timestamp*, so that day is only partially present in
    /// the store: rewriting it would replace a complete row (archived on an
    /// earlier build, when the cutoff was earlier still) with a partial one, on
    /// every build. So only days strictly after it are rewritten. The boundary
    /// day is written just once, when nothing is on file for it yet — on a
    /// first run a partial row beats losing the day outright.
    ///
    /// Days older than the boundary are left exactly as they are: they are the
    /// history the raw store can no longer reproduce.
    pub fn absorb(&mut self, live: BTreeMap<String, DayRow>, cutoff_date: chrono::NaiveDate) {
        let boundary = cutoff_date.format("%Y-%m-%d").to_string();
        for (date, row) in live {
            if date > boundary || !self.days.contains_key(&date) {
                self.days.insert(date, row);
            }
        }
    }
```

ISO dates compare correctly as strings (fixed-width, zero-padded, big-endian), so no date parsing is needed in the loop.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo test --lib rollup`
Expected: PASS, 12 tests. Then `cargo test --lib` → 111 passed, 2 ignored.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/rollup.rs
git commit -m "feat: absorb live day rows without clobbering complete history"
```

---

### Task 4: Read rows back — retroactive whitelist and pricing

**Files:**
- Modify: `src-tauri/src/parser.rs` (add `Agg::add_row`, near `impl Agg` around line 508)
- Test: `src-tauri/src/parser.rs` (inline `tests` module at the end)

**Interfaces:**
- Consumes: `DayRow`, `TokBits` (Task 1); the private `Agg` in `parser.rs`; `UserConfig::is_user_mcp` / `is_user_skill`; `Pricing::cost` / `cache_savings`; `normalize_model`.
- Produces: `fn add_row(&mut self, r: &DayRow, cfg: &UserConfig, pricing: &Pricing)` on `Agg`.

`add_row` lives in `parser.rs` because `Agg` is private to it. This is the read-time half of decisions 2 and 3: the whitelist and the price table are applied **here**, so both stay retroactive for archived days.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module at the end of `src-tauri/src/parser.rs`:

```rust
    use crate::rollup::{DayRow, TokBits};

    fn day_row() -> DayRow {
        let mut r = DayRow::new("2026-01-05");
        r.models.insert(
            "claude-opus-5-20260101".to_string(),
            TokBits { input: 1_000_000.0, cc: 0.0, cr: 0.0, out: 100_000.0, requests: 3 },
        );
        r.mcp.insert("mcp__github".to_string(), 4);
        r.mcp.insert("mcp__not-installed".to_string(), 9);
        r.skills.insert("gstack:review".to_string(), 2);
        r.tools.insert("Read".to_string(), 5);
        r.tools.insert("mcp__github".to_string(), 4);
        r.sessions = 2;
        r
    }

    fn cfg_with(mcp: &[&str], skills: &[&str]) -> UserConfig {
        UserConfig {
            mcp_servers: mcp.iter().map(|s| s.to_string()).collect(),
            skills: skills.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn an_archived_row_is_filtered_by_the_current_whitelist_on_read() {
        // The whole point of archiving names unfiltered: installing an MCP
        // server must make past calls count, even for days the raw store can no
        // longer reproduce.
        let mut agg = Agg::default();
        agg.add_row(&day_row(), &cfg_with(&["mcp__github"], &["review"]), &Pricing::empty());

        assert_eq!(agg.mcp_calls, 4); // the un-installed server contributes nothing
        assert_eq!(agg.mcp_counts.get("mcp__github"), Some(&4));
        assert_eq!(agg.mcp_counts.get("mcp__not-installed"), None);
        assert_eq!(agg.skill_calls, 2);
        assert_eq!(agg.skill_counts.get("review"), Some(&2));
    }

    #[test]
    fn a_newly_installed_server_retroactively_counts_in_an_archived_row() {
        let mut agg = Agg::default();
        agg.add_row(
            &day_row(),
            &cfg_with(&["mcp__github", "mcp__not-installed"], &[]),
            &Pricing::empty(),
        );
        assert_eq!(agg.mcp_calls, 13);
    }

    #[test]
    fn an_archived_rows_tools_drop_mcp_entries() {
        // mcp__ calls have their own server-grouped view; counting them again
        // under tools would duplicate them, exactly as compute_event avoids.
        let mut agg = Agg::default();
        agg.add_row(&day_row(), &cfg_with(&[], &[]), &Pricing::empty());

        assert_eq!(agg.tool_counts.get("Read"), Some(&5));
        assert_eq!(agg.tool_counts.get("mcp__github"), None);
    }

    #[test]
    fn an_archived_row_groups_tokens_under_the_normalized_model_name() {
        let mut agg = Agg::default();
        agg.add_row(&day_row(), &cfg_with(&[], &[]), &Pricing::empty());

        assert_eq!(agg.requests, 3);
        assert_eq!(agg.sessions_count, 2);
        assert_eq!(agg.input, 1_000_000.0);
        assert_eq!(agg.output, 100_000.0);
        // The dated release merges into its base model for display.
        assert!(agg.model_tok.contains_key("claude-opus-5"));
        assert!(!agg.model_tok.contains_key("claude-opus-5-20260101"));
        // No price table → cost unknown, and the model is marked unpriced.
        assert_eq!(agg.cost, 0.0);
        assert_eq!(agg.model_priced.get("claude-opus-5"), Some(&false));
    }
```

`Agg::sessions` is a `HashSet<String>` that a row cannot populate (a row stores only a count). Add a sibling counter rather than inventing synthetic ids, which would be indistinguishable from real ones if the two sources ever mixed.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo test --lib parser`
Expected: FAIL — `no method named `add_row` found` and `no field `sessions_count``.

- [ ] **Step 3: Write the minimal implementation**

In `src-tauri/src/parser.rs`, add a field to `struct Agg` (after `sessions: HashSet<String>`):

```rust
    /// Sessions contributed by archived rows, which carry a count rather than
    /// the ids. Kept separate from `sessions` so the two are never conflated:
    /// a synthetic id would be indistinguishable from a real one.
    sessions_count: u64,
```

Change `Agg::metrics` so the two sources add up. Replace:

```rust
            sessions: self.sessions.len() as u64,
```

with:

```rust
            sessions: self.sessions.len() as u64 + self.sessions_count,
```

Then add `add_row` to the `impl Agg` block, right after `fn add`:

```rust
    /// Fold one archived day into this aggregate.
    ///
    /// This is where the archive's two read-time contracts are honoured: MCP and
    /// skill names were stored unfiltered, so the *current* whitelist applies
    /// here; per-model tokens were stored raw, so the *current* price table
    /// applies here. Both therefore stay retroactive for days the raw event
    /// store can no longer reproduce.
    fn add_row(&mut self, r: &DayRow, cfg: &UserConfig, pricing: &Pricing) {
        for (raw, b) in &r.models {
            let model = normalize_model(raw);
            let cost = pricing
                .cost(raw, b.input, b.out, b.cc, b.cr)
                .or_else(|| pricing.cost(&model, b.input, b.out, b.cc, b.cr));
            let savings = pricing
                .cache_savings(raw, b.cr)
                .or_else(|| pricing.cache_savings(&model, b.cr))
                .unwrap_or(0.0);

            self.input += b.input;
            self.cache += b.cc + b.cr;
            self.output += b.out;
            self.cost += cost.unwrap_or(0.0);
            self.savings += savings;
            self.requests += b.requests;

            let tok = b.input + b.cc + b.cr + b.out;
            *self.model_tok.entry(model.clone()).or_default() += tok;
            *self.model_cost.entry(model.clone()).or_default() += cost.unwrap_or(0.0);
            *self.model_priced.entry(model).or_default() |= cost.is_some();
        }

        self.sessions_count += r.sessions;
        self.subagent_tok += r.subagent;
        self.tool_results += r.tool_results;
        self.tool_errors += r.tool_errors;

        // mcp__ calls have their own server-grouped view; including them here
        // would double-count them, exactly as compute_event avoids.
        for (name, c) in &r.tools {
            if !name.starts_with("mcp__") {
                *self.tool_counts.entry(name.clone()).or_default() += c;
            }
        }
        for (name, c) in &r.mcp {
            if cfg.is_user_mcp(name) {
                self.mcp_calls += c;
                *self.mcp_counts.entry(name.clone()).or_default() += c;
            }
        }
        for (name, c) in &r.skills {
            if cfg.is_user_skill(name) {
                self.skill_calls += c;
                let short = name.rsplit(':').next().unwrap_or(name).to_string();
                *self.skill_counts.entry(short).or_default() += c;
            }
        }

        for (name, (tok_m, cost)) in &r.projects {
            *self.project_tok.entry(name.clone()).or_default() += tok_m * 1e6;
            *self.project_cost.entry(name.clone()).or_default() += cost;
        }
        for (name, (tok_m, cost)) in &r.branches {
            *self.branch_tok.entry(name.clone()).or_default() += tok_m * 1e6;
            *self.branch_cost.entry(name.clone()).or_default() += cost;
        }
        for (name, (tok_m, cost)) in &r.accounts {
            *self.account_tok.entry(name.clone()).or_default() += tok_m * 1e6;
            *self.account_cost.entry(name.clone()).or_default() += cost;
        }
    }
```

Add the import at the top of `parser.rs`, next to the existing `use crate::store::{RawEvent, Store};`:

```rust
use crate::rollup::{Archive, DayRow};
```

(`Archive` is used in Task 5; importing both now keeps the import list stable.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo test --lib parser`
Expected: PASS. Then `cargo test --lib` → 115 passed, 2 ignored.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/parser.rs
git commit -m "feat: read archived days back through the current whitelist and prices"
```

---

### Task 5: The all-time report and its command

**Files:**
- Modify: `src-tauri/src/model.rs` (add `AllTimeReport`)
- Modify: `src-tauri/src/parser.rs` (persist rows in `account_events`; add `build_all_time`)
- Modify: `src-tauri/src/lib.rs` (add `get_all_time`, register it)
- Test: `src-tauri/src/parser.rs` (inline tests)

**Interfaces:**
- Consumes: everything from Tasks 1-4.
- Produces: `model::AllTimeReport`; `parser::build_all_time(account_id: &str) -> AllTimeReport`; Tauri command `get_all_time(account: String) -> AllTimeReport`.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src-tauri/src/parser.rs`:

```rust
    #[test]
    fn all_time_extras_describe_the_archived_range() {
        let mut archive = Archive::default();
        for (date, out) in [
            ("2026-01-05", 10.0),
            ("2026-01-06", 90.0), // the biggest day
            ("2026-01-07", 20.0),
            // a gap on the 8th breaks the streak
            ("2026-01-09", 30.0),
        ] {
            let mut r = DayRow::new(date);
            r.models.insert(
                "claude-opus-5".to_string(),
                TokBits { input: 0.0, cc: 0.0, cr: 0.0, out, requests: 1 },
            );
            archive.days.insert(date.to_string(), r);
        }

        let x = all_time_extras(&archive);
        assert_eq!(x.first, "2026-01-05");
        assert_eq!(x.last, "2026-01-09");
        assert_eq!(x.active_days, 4);
        assert_eq!(x.longest_streak, 3); // 05, 06, 07
        assert_eq!(x.biggest_day, Some(("2026-01-06".to_string(), 90.0 / 1e6)));
    }

    #[test]
    fn all_time_extras_of_an_empty_archive_are_empty_not_zeroed_dates() {
        let x = all_time_extras(&Archive::default());
        assert_eq!(x.first, "");
        assert_eq!(x.last, "");
        assert_eq!(x.active_days, 0);
        assert_eq!(x.longest_streak, 0);
        assert_eq!(x.biggest_day, None);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo test --lib parser`
Expected: FAIL — `cannot find function `all_time_extras``.

- [ ] **Step 3: Write the minimal implementation**

**3a.** In `src-tauri/src/model.rs`, add after `PeriodReport`:

```rust
/// The all-time report: an ordinary `PeriodReport` (so every existing chart
/// component works unchanged, with `series` holding monthly buckets) plus the
/// facts that only exist at this scale. `delta_*` and `trend` on the inner
/// report are left at their defaults and are not rendered — the fields here
/// replace them rather than faking a comparison against a previous "period".
#[derive(Debug, Clone, Serialize)]
pub struct AllTimeReport {
    pub report: PeriodReport,
    /// ISO date of the first/last day with any usage; empty when there is none.
    pub first: String,
    pub last: String,
    #[serde(rename = "activeDays")]
    pub active_days: u64,
    /// (ISO date, M tokens) of the single biggest day.
    #[serde(rename = "biggestDay")]
    pub biggest_day: Option<(String, f64)>,
    #[serde(rename = "longestStreak")]
    pub longest_streak: u64,
}
```

**3b.** In `src-tauri/src/parser.rs`, persist rows during ingest. In `account_events`, after the existing `store.save(&a.id);` block and before `let cfg = (d.load_config)(a);`, the project memo is needed by both the rollup and the event mapping, so hoist it. Replace the body from `let cfg = ...` through the `let events = ...` binding with:

```rust
    let cfg = (d.load_config)(a);
    // Resolve each event's project to its git-repo root, memoized per unique cwd
    // so the (filesystem-backed) walk-up runs once per directory, not per event.
    let mut proj_memo: HashMap<String, String> = HashMap::new();
    let mut resolve = |cwd: &str| -> String {
        proj_memo
            .entry(cwd.to_string())
            .or_insert_with(|| resolve_project(cwd))
            .clone()
    };

    // Fold this build's live days into the durable archive before the events are
    // mapped. `cutoff` is a timestamp, so its own day is only partly in the
    // store; `absorb` is what keeps that from overwriting a complete row.
    let cutoff_date = DateTime::from_timestamp_millis(cutoff)
        .unwrap_or_default()
        .with_timezone(&Local)
        .date_naive();
    let rows = crate::rollup::rows_from_events(&store.events, &mut resolve, &a.label, pricing);
    let mut archive = Archive::load(&a.id);
    archive.absorb(rows, cutoff_date);
    archive.save(&a.id);

    let events = store
        .events
        .iter()
        .map(|r| {
            let mut e = compute_event(r, &cfg, pricing);
            e.project = resolve(&r.cwd);
            e.account = a.label.clone();
            e
        })
        .collect();
```

Change `account_events`'s return type to also hand back the archive, since `build_all_time` needs it and it has just been loaded:

```rust
) -> (
    Vec<Event>,
    HashSet<String>,
    HashSet<String>,
    Option<crate::model::QuotaSnapshot>,
    Archive,
) {
```

and return `(events, cfg.mcp_servers, cfg.skills, quota, archive)`. Update the three existing call sites (`build_workspace`, `build_period`, and any other) to bind the extra element with `_` where unused.

**3c.** Add the extras helper and the report builder to `src-tauri/src/parser.rs`:

```rust
/// The facts that only exist at all-time scale, derived from the archive.
struct AllTimeExtras {
    first: String,
    last: String,
    active_days: u64,
    biggest_day: Option<(String, f64)>,
    longest_streak: u64,
}

/// Days with any usage, the biggest of them, and the longest unbroken run.
/// A row with no tokens is not an active day: the archive can hold one for a
/// day that saw only slash-command records.
fn all_time_extras(archive: &Archive) -> AllTimeExtras {
    let mut active: Vec<(chrono::NaiveDate, f64)> = Vec::new();
    for (date, r) in &archive.days {
        let tok: f64 = r
            .models
            .values()
            .map(|b| b.input + b.cc + b.cr + b.out)
            .sum();
        if tok <= 0.0 {
            continue;
        }
        if let Ok(d) = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") {
            active.push((d, tok / 1e6));
        }
    }
    active.sort_by_key(|(d, _)| *d);

    let biggest = active
        .iter()
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(d, t)| (iso(*d), r2(*t)));

    let mut longest = 0u64;
    let mut run = 0u64;
    let mut prev: Option<chrono::NaiveDate> = None;
    for (d, _) in &active {
        run = match prev {
            Some(p) if *d == p + Duration::days(1) => run + 1,
            _ => 1,
        };
        longest = longest.max(run);
        prev = Some(*d);
    }

    AllTimeExtras {
        first: active.first().map(|(d, _)| iso(*d)).unwrap_or_default(),
        last: active.last().map(|(d, _)| iso(*d)).unwrap_or_default(),
        active_days: active.len() as u64,
        biggest_day: biggest,
        longest_streak: longest,
    }
}

/// Build the all-time report for one account id, or `"all"` for every account
/// summed. Reads only the durable archives — every live day was absorbed into
/// them by `account_events`, so the archive alone is the complete picture and
/// no day can be counted twice.
pub fn build_all_time(account_id: &str) -> AllTimeReport {
    let _guard = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cutoff = (Local::now() - Duration::days(210)).timestamp_millis();
    let pricing = Pricing::shared();

    let mut agg = Agg::default();
    let mut merged = Archive::default();
    let mut servers: HashSet<String> = HashSet::new();
    let mut skills: HashSet<String> = HashSet::new();

    for (d, a) in crate::agents::discover_all() {
        if account_id != "all" && a.id != account_id {
            continue;
        }
        // Runs ingest + absorb, so the archive read below is current.
        let (_ev, srv, sk, _q, archive) = account_events(d, &a, &pricing, cutoff);
        let cfg = (d.load_config)(&a);
        for row in archive.days.values() {
            agg.add_row(row, &cfg, &pricing);
        }
        // For the extras (first/last/streak/biggest) and the monthly bars the
        // accounts' days union: a day is active if any account worked that day.
        for (date, row) in archive.days {
            let e = merged
                .days
                .entry(date.clone())
                .or_insert_with(|| DayRow::new(&date));
            for (m, b) in &row.models {
                let t = e.models.entry(m.clone()).or_default();
                t.input += b.input;
                t.cc += b.cc;
                t.cr += b.cr;
                t.out += b.out;
                t.requests += b.requests;
            }
            for (i, v) in row.hourly.iter().take(24).enumerate() {
                e.hourly[i] += v;
            }
        }
        servers.extend(srv);
        skills.extend(sk);
    }

    let x = all_time_extras(&merged);
    let series = monthly_series(&merged);
    let mut metrics = agg.metrics(0.0, 0.0);
    metrics.servers = servers.len() as u64;
    metrics.skills = skills.len() as u64;

    // `merged` already carries each day's summed histogram, so this is the
    // cross-day total.
    let mut hourly = vec![0.0f64; 24];
    for row in merged.days.values() {
        for (i, v) in row.hourly.iter().take(24).enumerate() {
            hourly[i] += v;
        }
    }

    let range = if x.first.is_empty() {
        "No usage yet".to_string()
    } else {
        format!("All time · since {}", x.first)
    };

    AllTimeReport {
        report: PeriodReport {
            metrics,
            series,
            models: agg.models(),
            projects: Agg::named_tokens(&agg.project_tok, &agg.project_cost),
            branches: Agg::named_tokens(&agg.branch_tok, &agg.branch_cost),
            accounts: Agg::named_tokens(&agg.account_tok, &agg.account_cost),
            tools: Agg::named(&agg.tool_counts),
            mcp: Agg::named(&agg.mcp_counts),
            skills: Agg::named(&agg.skill_counts),
            req_trend: Vec::new(),
            cost_trend: Vec::new(),
            hourly,
            range,
            // Deliberately empty: there is no previous all-time to trend against.
            trend: Vec::new(),
        },
        first: x.first,
        last: x.last,
        active_days: x.active_days,
        biggest_day: x.biggest_day,
        longest_streak: x.longest_streak,
    }
}

/// One bar per calendar month spanned by the archive, oldest→newest, with a
/// sparse axis label so a multi-year range stays readable.
fn monthly_series(archive: &Archive) -> Vec<SeriesPoint> {
    let mut by_month: std::collections::BTreeMap<String, (f64, f64, f64)> =
        std::collections::BTreeMap::new();
    for (date, r) in &archive.days {
        let key = date[..7].to_string(); // "yyyy-mm"
        let e = by_month.entry(key).or_default();
        for b in r.models.values() {
            e.0 += b.input / 1e6;
            e.1 += (b.cc + b.cr) / 1e6;
            e.2 += b.out / 1e6;
        }
    }
    let n = by_month.len();
    by_month
        .into_iter()
        .enumerate()
        .map(|(i, (key, (input, cache, output)))| {
            let (y, m) = key.split_at(4);
            let mi: usize = m[1..].parse::<usize>().unwrap_or(1) - 1;
            // Label roughly six ticks regardless of range length.
            let every = (n / 6).max(1);
            SeriesPoint {
                label: if i % every == 0 { MONTHS[mi].to_string() } else { String::new() },
                full: format!("{} {}", MONTHS[mi], y),
                input,
                cache,
                output,
                date: format!("{key}-01"),
            }
        })
        .collect()
}
```

**3d.** In `src-tauri/src/lib.rs`, add the command next to `get_period` (after line 763):

```rust
/// The all-time report for `account` ("all" or an account id): lifetime totals,
/// monthly history, and records, read from the durable rollup archive.
#[tauri::command]
async fn get_all_time(account: String) -> model::AllTimeReport {
    let acc = account.clone();
    tauri::async_runtime::spawn_blocking(move || parser::build_all_time(&account))
        .await
        .unwrap_or_else(|_| parser::build_all_time(&acc))
}
```

and register it in `invoke_handler` (around line 923), after `get_period,`:

```rust
            get_all_time,
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo test --lib`
Expected: PASS — 117 passed, 2 ignored. Also `cargo build` must be clean of warnings about unused variables at the updated `account_events` call sites.

- [ ] **Step 5: Verify against real data**

Run: `cd src-tauri && cargo run --example dump | head -5`
Expected: valid JSON, exit 0. This exercises the new `account_events` signature and the archive write against the real logs on this machine. Confirm `~/Library/Caches/tokenscope/rollup-*.json` now exists, one per account.

**Note:** `dump` prints the *dashboard*, not the all-time report, and it does not load real prices — see the repo's known limitation. Use it as a smoke test for "does it run and write the archive", not to verify cost figures.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/model.rs src-tauri/src/parser.rs src-tauri/src/lib.rs
git commit -m "feat: build the all-time report from the durable rollup archive"
```

---

### Task 6: Frontend data layer

**Files:**
- Modify: `src/data.ts:20-24` (after `PeriodReport`), and near `fetchPeriod` (line ~52)

**Interfaces:**
- Consumes: the `get_all_time` command (Task 5); the existing `PeriodReport` interface.
- Produces: `export interface AllTimeReport`; `export async function fetchAllTime(account: string): Promise<AllTimeReport>`.

- [ ] **Step 1: Add the interface**

In `src/data.ts`, immediately after the `PeriodReport` interface:

```ts
// All-time: an ordinary PeriodReport (series = monthly buckets) plus the facts
// that only exist at this scale. The inner report's delta/trend fields are
// deliberately empty — these replace them.
export interface AllTimeReport {
  report: PeriodReport;
  first: string; last: string;       // ISO; "" when there is no usage at all
  activeDays: number;
  biggestDay: [string, number] | null; // (ISO date, M tokens)
  longestStreak: number;
}
```

- [ ] **Step 2: Add the fetcher**

In `src/data.ts`, immediately after `fetchPeriod`:

```ts
// Fetch the all-time report for an account ("all" or an id). Read from the
// durable rollup archive, so it covers history the 210-day event store has
// already pruned.
export async function fetchAllTime(account: string): Promise<AllTimeReport> {
  const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
  if (inTauri) return invoke<AllTimeReport>("get_all_time", { account });
  // Dev fallback: the static snapshot has no archive, so stand in with the
  // month report and derive the range from the heatmap.
  const res = await fetch("/dev-dashboard.json");
  if (!res.ok) throw new Error("no dev snapshot");
  const dash: Dashboard = await res.json();
  const active = dash.heatmap.filter((d) => d.tokens > 0);
  const biggest = active.reduce<HeatDay | null>((b, d) => (b && b.tokens >= d.tokens ? b : d), null);
  return {
    report: dash.month,
    first: active[0]?.date ?? "",
    last: active[active.length - 1]?.date ?? "",
    activeDays: active.length,
    biggestDay: biggest ? [biggest.date, biggest.tokens] : null,
    longestStreak: activeStreak(dash.heatmap),
  };
}
```

- [ ] **Step 3: Typecheck**

Run: `pnpm build`
Expected: exit 0, no TypeScript errors.

- [ ] **Step 4: Commit**

```bash
git add src/data.ts
git commit -m "feat: add the all-time report type and its fetcher"
```

---

### Task 7: The All time page

**Files:**
- Modify: `src/App.tsx` (new `AllTimePage`; `Panel` renders it; `period` state widens; `Segmented` gains a fourth item)
- Modify: `src/charts.tsx:18` (`Segmented` default `items`) — **only if** the default needs changing; prefer passing `items` explicitly from `App.tsx` and leaving the default alone.

**Interfaces:**
- Consumes: `AllTimeReport`, `fetchAllTime` (Task 6); the existing `BarChart`, `BarList`, `TokenBarList`, `CostDonut`, `Segmented`, `MiniStat`, `Label`, `SectionRule`, `fmtTokens`, `fmtMoney`, `fmtInt`, `fmtHeatDate`, `peakHours`, `fmtHourRange`.
- Produces: nothing consumed by later tasks.

- [ ] **Step 1: Widen the period state and add the segment**

In `src/App.tsx`:

```ts
const [period, setPeriod] = useState<"Day" | "Week" | "Month" | "All">("Week");
```

Update `changePeriod`'s cast to `"Day" | "Week" | "Month" | "All"`, and make it a no-op for date navigation when `All` is picked (there is no reference date to keep):

```ts
  const changePeriod = (p: string) => {
    setPeriod(p as "Day" | "Week" | "Month" | "All");
    setFetchedPeriod(null); // force a refetch at the new granularity
    if (p === "All") { setRefDate(null); return; }
    if (refDate && isCurrentPeriod(refDate, p)) setRefDate(null);
  };
```

Add all-time state and its fetch effect, next to the existing `fetchedPeriod` effect:

```ts
  const [allTime, setAllTime] = useState<AllTimeReport | null>(null);
  useEffect(() => {
    if (period !== "All") return;
    let cancelled = false;
    setAllTime(null); // show the loading state while the account switch lands
    fetchAllTime(activeTab)
      .then((r) => { if (!cancelled) setAllTime(r); })
      .catch(() => {});
    return () => { cancelled = true; };
  }, [period, activeTab]);
```

Pass `allTime={allTime}` down to `<Panel>`, and widen `Panel`'s `period` prop type to `"Day" | "Week" | "Month" | "All"`.

In `Panel`'s header, pass the four-item list explicitly so `charts.tsx` keeps its existing default:

```tsx
<Segmented value={period} items={["Day", "Week", "Month", "All"]} theme={t} onSelect={onPeriod} />
```

`report` still needs a valid value while `All` is selected. In `App`, leave the existing `report` computation untouched — `period === "All"` falls through to the week report, which `Panel` simply does not render in that mode.

- [ ] **Step 2: Render the page**

In `src/App.tsx`, add the component above `Panel`:

```tsx
// The all-time page. A different question from Day/Week/Month — "what has this
// cost me, ever" — so it gets its own body rather than a wider window on the
// period layout: no date navigation, no delta badge, and records instead of a
// comparison against a previous period.
function AllTimePage({ a, t, ramp }: { a: AllTimeReport; t: Theme; ramp: string[] }) {
  const M = a.report.metrics;
  const models = a.report.models.map((m, i) => ({ ...m, color: i < ramp.length ? ramp[i] : PRESET_OVERFLOW }));
  const costModels = models.filter((m) => m.cost > 0);
  const peak = peakHours(a.report.hourly);
  if (!a.first) {
    return <div style={{ font: `500 11px ${t.mono}`, color: t.faint, padding: "18px 0" }}>No usage recorded yet.</div>;
  }
  return (
    <div>
      <div style={{ font: `600 10px ${t.ui}`, color: t.dim, letterSpacing: ".05em", textTransform: "uppercase" }}>
        All time · since {fmtHeatDate(a.first)}
      </div>
      <div style={{ display: "flex", alignItems: "baseline", gap: 10, marginTop: 6 }}>
        <span style={{ font: `600 30px/1 ${t.display}`, color: t.text }}>{fmtTokens(M.totalTokens)}</span>
        <span style={{ font: `600 15px ${t.mono}`, color: t.accent }}>{fmtMoney(M.cost)}</span>
      </div>

      <SectionRule t={t} />
      <Label t={t}>By month</Label>
      <div style={{ marginTop: 8 }}>
        <BarChart data={a.report.series} theme={t} accent={t.accent} accentSoft={t.accentSoft} />
      </div>

      <SectionRule t={t} />
      <Label t={t}>Records</Label>
      <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 8, marginTop: 8 }}>
        <MiniStat label="Active days" value={fmtInt(a.activeDays)} theme={t} />
        <MiniStat label="Longest streak" value={`${fmtInt(a.longestStreak)}d`} theme={t} />
        <MiniStat
          label="Biggest day"
          value={a.biggestDay ? fmtTokens(a.biggestDay[1]) : "—"}
          sub={a.biggestDay ? fmtHeatDate(a.biggestDay[0]) : undefined}
          theme={t}
        />
        <MiniStat label="Sessions" value={fmtInt(M.sessions)} sub={`${fmtInt(M.requests)} requests`} theme={t} />
      </div>
      {peak && (
        <div style={{ font: `500 10px ${t.mono}`, color: t.faint, marginTop: 8 }}>
          Busiest hours {fmtHourRange(peak.start, peak.end)} · {Math.round(peak.share * 100)}% of all tokens
        </div>
      )}

      <SectionRule t={t} />
      <Label t={t}>Models</Label>
      <div style={{ marginTop: 6 }}>
        {models.map((m, i) => (
          <ModelRow key={m.name} m={m} max={Math.max(...models.map((x) => x.tokens), 1e-9)} theme={t}
            share={sharePcts(models.map((x) => x.tokens))[i]} />
        ))}
      </div>
      {costModels.length > 0 && (
        <div style={{ display: "flex", justifyContent: "center", marginTop: 10 }}>
          <CostDonut models={costModels} theme={t} palette={ramp} overflow={PRESET_OVERFLOW} />
        </div>
      )}

      {a.report.projects.length > 0 && (<>
        <SectionRule t={t} />
        <Label t={t}>Projects</Label>
        <div style={{ marginTop: 6 }}><TokenBarList items={a.report.projects} theme={t} accent={t.accent} /></div>
      </>)}
      {a.report.mcp.length > 0 && (<>
        <SectionRule t={t} />
        <Label t={t}>MCP calls</Label>
        <div style={{ marginTop: 6 }}><BarList items={a.report.mcp} theme={t} accent={t.accent} /></div>
      </>)}
      {a.report.skills.length > 0 && (<>
        <SectionRule t={t} />
        <Label t={t}>Skill calls</Label>
        <div style={{ marginTop: 6 }}><BarList items={a.report.skills} theme={t} accent={t.accent} /></div>
      </>)}
    </div>
  );
}
```

Import `AllTimeReport` and `fetchAllTime` from `./data`, and `fmtHeatDate` if not already imported.

- [ ] **Step 3: Branch the panel body**

In `Panel`, wrap the existing scrolling body. Immediately after the `<div style={{ padding: "14px 15px 15px" }}>` that opens the body, branch:

```tsx
{period === "All" ? (
  allTime
    ? <AllTimePage a={allTime} t={t} ramp={ramp} />
    : <div style={{ font: `500 11px ${t.mono}`, color: t.faint, padding: "18px 0" }}>Loading…</div>
) : (<>
  {/* ...the entire existing body, unchanged... */}
</>)}
```

The period navigation row, the hero and every existing section move inside the `else` branch. Nothing in the existing body changes other than its indentation — verify with `git diff --stat` that `App.tsx` shows no unintended deletions.

- [ ] **Step 4: Typecheck and run**

Run: `pnpm build`
Expected: exit 0.

Then run the app and check, on both a single-account and the `All` tab:
- the `All` segment appears and switches the body;
- the `‹ ›` navigation and the delta badge are gone in that mode;
- the month bars, records and lists render with real numbers;
- switching back to `Week` restores the normal panel intact;
- the page renders correctly in both light and dark themes.

- [ ] **Step 5: Commit**

```bash
git add src/App.tsx
git commit -m "feat: add the all-time analysis page to the panel"
```

---

### Task 8: Document the archive

**Files:**
- Modify: `README.md` (the data-sources table and the processing notes)
- Modify: `README-zh.md` (the mirrored sections)

- [ ] **Step 1: Update the English README**

Add a row to the data-sources table:

```markdown
| All-time daily rollup (survives the 210-day event prune) | `~/Library/Caches/tokenscope/rollup-<account>.json` |
```

And a short paragraph after the existing processing notes:

```markdown
The **All time** page reads a durable per-day rollup rather than the event
store, which keeps only the last 210 days. Each day is archived once the raw
store still fully covers it, and rewritten on every launch until it falls out of
that window. MCP/Skill names are archived unfiltered and per-model tokens are
archived raw, so installing an MCP server or a price refresh still applies
retroactively to archived days. What an archived day cannot do is drill down
below the hour histogram it stores.
```

- [ ] **Step 2: Mirror into README-zh.md**

Match the existing Chinese translation's tone and table format. Keep the file path and the `All time` UI label untranslated, as the file already does for other paths and labels.

- [ ] **Step 3: Commit**

```bash
git add README.md README-zh.md
git commit -m "docs: describe the all-time rollup archive and its limits"
```

---

## Self-Review

**Spec coverage:**

| Spec section | Task |
|---|---|
| Storage: `rollup.rs`, versioned, atomic | 1 |
| Row shape (`TokBits`, `DayRow`, `Archive`) | 1 |
| Savings not stored (re-derived) | 1 (absent field), 4 (`cache_savings` on read) |
| Unfiltered names / raw model tokens | 2 (write), 4 (read) |
| `Agg::add` accumulation rules mirrored | 2, 4 |
| Per-account archives, "All" summed on read | 1, 5 |
| The date-keyed disjointness invariant | 3 |
| Window boundary = cutoff date | 3, 5 |
| Backfill is free | 3 (`!contains_key` branch) |
| Sessions double-count across midnight | 1 (comment), 4 (`sessions_count`) |
| `AllTimeReport` wrapper | 5 |
| `get_all_time` command | 5 |
| Frontend types + fetcher | 6 |
| Fourth `Segmented` item, own page body | 7 |
| Nav/delta hidden not disabled | 7 |
| Testing list | 1, 2, 3, 4, 5 |

**Deviation from the spec, resolved in the plan's favour:** the spec described all-time as "archive rows + live aggregation" summed at read time. Task 3 makes it strictly simpler — live rows are absorbed *into* the archive during ingest, so `build_all_time` reads only the archive. The invariant the spec asked for (never count a day twice) becomes structural rather than a merge step, because `Archive::days` is keyed by date. This is a strengthening, not a change of behaviour.

**Sharp edge the spec did not name:** `prune_before` cuts on a timestamp, so the cutoff's own day is partial in the raw store. Task 3's `absorb` exists to stop that partial day from overwriting a complete archived row on every build. The spec's "window start" is therefore implemented as *strictly after* `cutoff_date`.

**Type consistency:** `DayRow`/`TokBits` field names are identical across Tasks 1-5. `Archive::days` is `BTreeMap<String, DayRow>` everywhere. `AllTimeReport`'s serde names (`activeDays`, `biggestDay`, `longestStreak`) match the TS interface in Task 6 exactly. `all_time_extras` returns `AllTimeExtras`, consumed only within Task 5.

**Fixed during self-review** (recorded so an executor does not "helpfully" revert them):

- `rows_from_events` takes `&mut dyn FnMut(&str) -> String`, not `&dyn Fn`. The caller's resolver memoizes into a `HashMap` and so captures it mutably; a `Fn` parameter would not compile.
- `Pricing` gains `#[derive(Default)]`. It has none today, so the `#[cfg(test)] empty()` constructor would not compile without it.
- The per-account union in `build_all_time` uses `or_insert_with` + a following loop rather than `and_modify(...).or_insert(row)`. The latter borrows `row` in the closure and moves it in the same statement, and temporaries live to the end of the statement — a move-out-of-borrowed error.
- `AllTimePage` takes no `dark` prop. It never used it; `tsconfig.json` has `noUnusedParameters: false`, so this would have passed typecheck as dead surface area.

**Known plan risk:** Task 5 step 3b restructures `account_events`'s return type, touching all its call sites. If the executor finds a call site this plan did not anticipate, update it to bind the fifth element and note it — do not change the return type back.
