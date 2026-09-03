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
use crate::pricing::{priced_cost, Pricing};
use crate::store::RawEvent;
use chrono::{DateTime, Local, Timelike};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

// Bump when a row's meaning changes. A mismatch discards the archive whole:
// up to 210 days rebuild from the raw store, and older history is lost, which
// is strictly better than silently misreading it.
pub const ROLLUP_VERSION: u32 = 1;

/// Raw token components for one model on one day, plus its request count.
/// Kept raw (absolute token counts, not M tokens or cost) so cost is re-derived
/// from the *current* price table on read.
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct TokBits {
    /// Absolute raw input tokens.
    pub input: f64,
    /// cache creation tokens (absolute raw count).
    pub cc: f64,
    /// cache read tokens (absolute raw count).
    pub cr: f64,
    /// Absolute raw output tokens.
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
    /// Every tool_use name in this message. Contains both `mcp__` and non-MCP entries;
    /// `mcp__` entries are dropped on read (never whitelist-filtered) so the MCP view
    /// isn't duplicated.
    #[serde(default)]
    pub tools: HashMap<String, u64>,
    /// Unfiltered MCP server names — the whitelist is applied on read.
    #[serde(default)]
    pub mcp: HashMap<String, u64>,
    /// Unfiltered Skill names — the whitelist is applied on read.
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
    /// Absolute raw tokens spent inside subagents (isSidechain).
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
    /// Local calendar date (ISO) of the last `absorb`, or "" if never. Lets the
    /// hot path skip a re-fold it does not need without risking a lost day —
    /// see `needs_absorb`. Empty on an archive written before this field
    /// existed, which reads as "behind today" and simply folds once.
    #[serde(default)]
    pub last_absorbed: String,
}

#[derive(Serialize)]
struct DocRef<'a> {
    version: u32,
    last_absorbed: &'a str,
    days: &'a BTreeMap<String, DayRow>,
}

#[derive(Deserialize)]
struct Doc {
    version: u32,
    /// Added after `ROLLUP_VERSION` 1 shipped. Deliberately NOT a version bump:
    /// a bump discards every archived day, and this field defaults harmlessly.
    #[serde(default)]
    last_absorbed: String,
    days: BTreeMap<String, DayRow>,
}

/// Atomically replace `path`'s contents: write a sibling temp file, then rename
/// over the target (same-volume rename is atomic on Windows and Unix). Avoids
/// the half-written/truncated JSON that a crash mid-`fs::write` would leave.
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
            .map(|d| Archive {
                days: d.days,
                last_absorbed: d.last_absorbed,
            })
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
            last_absorbed: &self.last_absorbed,
            days: &self.days,
        };
        if let Ok(t) = serde_json::to_string(&doc) {
            let _ = write_atomic(&dir.join(format!("rollup-{id}.json")), t.as_bytes());
        }
    }

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

    /// Whether durability *requires* re-folding the raw store into this archive
    /// on the current pass, for a caller that does not itself need to read the
    /// archive. `today` is the local calendar date, ISO; `pruned` is whether
    /// `Store::prune_before` dropped anything this pass.
    ///
    /// Re-folding is the expensive half of a refresh (every retained event, on
    /// every 30s poll *and* every 400ms-debounced watcher tick), and the fold is
    /// idempotent — the same events yield the same rows — so the only question
    /// is whether skipping it could ever lose a day.
    ///
    /// It cannot. A day's events sit in the raw store for `RETENTION_DAYS`, and
    /// `absorb` rewrites every day strictly after the prune cutoff, which
    /// advances one day at a time. So a day D is eligible for absorption on
    /// every fold from the day it happens until the cutoff reaches it —
    /// hundreds of chances — and this returns true at least once per local
    /// calendar day (`last_absorbed` is behind `today` until the day's first
    /// fold). D is therefore archived long before it becomes the boundary day,
    /// and its row is then frozen exactly as `absorb` intends. `pruned` forces
    /// the fold on the pass that actually moves the cutoff, and an empty `days`
    /// forces it on a first run (or after a version reset), when there is no
    /// archived history to protect at all.
    ///
    /// A price refresh or a whitelist change is deliberately NOT a trigger: rows
    /// keep raw per-model tokens and unfiltered names precisely so both apply at
    /// read time in `Agg::add_row`.
    pub fn needs_absorb(&self, today: &str, pruned: bool) -> bool {
        self.days.is_empty() || pruned || self.last_absorbed.as_str() < today
    }
}

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
        // Same two-step lookup the read paths use (raw id, then normalized):
        // these costs are frozen into the archive, so a miss here is permanent.
        let cost = priced_cost(pricing, &e.model, e.in_tok, e.out_tok, e.cc, e.cr).unwrap_or(0.0);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::ModelPrice;
    use chrono::NaiveDate;

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
    fn an_archived_cost_prices_a_dated_id_against_an_undated_table_entry() {
        // The table holds only the undated base model — exactly the shape of the
        // built-in snapshot `Pricing::shared()` serves until the async price
        // loader lands. `Pricing::lookup` never strips `-YYYYMMDD`, so a one-step
        // lookup on the raw id misses and freezes this day's project, branch and
        // account costs at $0 forever (this file is never pruned), while the
        // headline and per-model costs — re-derived on read via the two-step
        // lookup — come out correct. `priced_cost` is what keeps the two agreeing.
        let pricing = Pricing::with_exact(&[(
            "claude-opus-5",
            ModelPrice { input: 2e-6, output: 10e-6, cache_create: 0.0, cache_read: 0.0 },
        )]);
        let mut proj = |_: &str| "repo".to_string();
        let rows = rows_from_events(
            &[raw(TS, "claude-opus-5-20260101", "s1")],
            &mut proj,
            "Work",
            &pricing,
        );
        let (_, r) = rows.iter().next().unwrap();

        let want = 100.0 * 2e-6 + 10.0 * 10e-6; // 100 input, 10 output
        assert!(want > 0.0);
        assert!(
            (r.projects["repo"].1 - want).abs() < 1e-12,
            "project cost {} != {want}",
            r.projects["repo"].1
        );
        assert!((r.branches["main"].1 - want).abs() < 1e-12);
        assert!((r.accounts["Work"].1 - want).abs() < 1e-12);
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
        // Same session on two different days, plus two events with the same
        // session on day 1 to verify per-day deduplication.
        let rows = rows_from_events(
            &[
                raw(TS, "m", "s1"),
                raw(TS + 3600_000, "m", "s1"),  // Second s1 event on same day
                raw(TS + 7200_000, "m", "s2"),   // Different session, same day
                raw(TS + day_ms, "m", "s1"),    // s1 again on day 2
            ],
            &mut proj,
            "Work",
            &p,
        );
        assert_eq!(rows.len(), 2);
        let days: Vec<_> = rows.values().collect();
        // Day 1: s1 appears twice but counts as one session; s2 appears once → 2 sessions total
        assert_eq!(days[0].sessions, 2);
        // Day 2: s1 appears once → 1 session
        assert_eq!(days[1].sessions, 1);
    }

    fn row(date: &str, out: f64) -> DayRow {
        let mut r = DayRow::new(date);
        r.models.insert(
            "claude-opus-5".to_string(),
            TokBits { input: 10.0, cc: 0.0, cr: 0.0, out, requests: 1 },
        );
        r
    }

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

    #[test]
    fn a_second_pass_on_the_same_day_with_nothing_new_does_not_re_absorb() {
        // The hot path: the 30s poll and the 400ms-debounced watcher both land
        // here, and re-folding every retained event on each is the single most
        // expensive thing a refresh does.
        let mut a = Archive::default();
        a.absorb(live(&[("2026-03-10", 5.0)]), d("2026-01-01"));
        a.last_absorbed = "2026-03-10".to_string();

        assert!(!a.needs_absorb("2026-03-10", false));
    }

    #[test]
    fn a_first_run_with_an_empty_archive_absorbs() {
        // Nothing on file yet, so there is no history to protect and every day
        // in the raw store needs archiving now.
        let a = Archive::default();
        assert!(a.needs_absorb("2026-03-10", false));
    }

    #[test]
    fn a_date_rollover_absorbs_again() {
        // Yesterday's fold cannot contain today's events. One fold per local
        // calendar day is what makes the skip above safe.
        let mut a = Archive::default();
        a.absorb(live(&[("2026-03-10", 5.0)]), d("2026-01-01"));
        a.last_absorbed = "2026-03-10".to_string();

        assert!(a.needs_absorb("2026-03-11", false));
    }

    #[test]
    fn a_prune_forces_a_re_absorb_on_the_pass_that_moved_the_cutoff() {
        // The cutoff just advanced, so the day that fell out of the raw store is
        // making its last appearance — archive it before it is gone.
        let mut a = Archive::default();
        a.absorb(live(&[("2026-03-10", 5.0)]), d("2026-01-01"));
        a.last_absorbed = "2026-03-10".to_string();

        assert!(a.needs_absorb("2026-03-10", true));
    }

    #[test]
    fn an_archive_written_before_last_absorbed_existed_folds_once_and_settles() {
        // `#[serde(default)]`, not a ROLLUP_VERSION bump: an existing archive
        // keeps every day it holds and simply re-folds on its first pass.
        let dir = std::env::temp_dir().join(format!("ts-roll-abs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        fs::write(
            dir.join("rollup-acct.json"),
            serde_json::json!({
                "version": ROLLUP_VERSION,
                "days": { "2026-01-05": { "date": "2026-01-05" } }
            })
            .to_string(),
        )
        .unwrap();

        let mut a = Archive::load_from(&dir, "acct");
        assert_eq!(a.days.len(), 1, "history survives the added field");
        assert_eq!(a.last_absorbed, "");
        assert!(a.needs_absorb("2026-03-10", false));

        a.last_absorbed = "2026-03-10".to_string();
        a.save_to(&dir, "acct");
        let back = Archive::load_from(&dir, "acct");
        assert_eq!(back.last_absorbed, "2026-03-10");
        assert!(!back.needs_absorb("2026-03-10", false));

        let _ = fs::remove_dir_all(&dir);
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
