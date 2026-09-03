// Parse ~/.claude/projects/**/*.jsonl, dedupe assistant messages by id,
// classify tool calls (user-installed MCP / Skill only), and aggregate
// into Day / Week / Month reports + a daily heatmap.
use crate::config::UserConfig;
use crate::model::*;
use crate::pricing::Pricing;
use crate::rollup::{Archive, DayRow};
use crate::store::{RawEvent, Store};
use chrono::{DateTime, Datelike, Duration, Local, Timelike};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

// Serializes dashboard builds so the background refresh thread and the command
// handler never touch the incremental cache files concurrently.
static BUILD_LOCK: Mutex<()> = Mutex::new(());

// One assistant API response, with config + pricing applied (derived per request
// from a RawEvent, since user config / prices / time windows can all change).
struct Event {
    ts: DateTime<Local>,
    session: String,
    model: String,
    input: f64,  // raw tokens, uncached new input only
    cache: f64,  // raw tokens, cache creation + read
    output: f64, // raw tokens
    cost: f64,   // USD (differentiated by token type), 0 if unknown model
    savings: f64, // USD saved by cache reads on this call (0 if unknown model)
    priced: bool, // whether a price was found for this model
    project: String, // cwd basename ("" if unknown)
    branch: String,  // git branch ("" if unknown)
    account: String, // owning account label (set by account_events)
    tools: Vec<String>, // all tool_use names in this msg (mcp__ excluded here)
    sidechain: bool,    // ran inside a subagent
    tool_results: u64,  // tool_result blocks (reliability denominator)
    tool_errors: u64,   // of those, is_error count
    mcp: Vec<String>,   // user-installed server names called in this msg
    skills: Vec<String>, // user-installed skill names called in this msg
}

// Top-5 models keep the green/slate scheme; everything beyond is uniform gray.
const PALETTE: &[&str] = &["#1f9d63", "#34c27e", "#6ad0a0", "#a7e3c5", "#4b5a52"];
const OVERFLOW_GRAY: &str = "#79817b";

/// Strip a trailing "-YYYYMMDD" date suffix so dated releases merge into
/// their base model (e.g. "claude-haiku-4-5-20251001" → "claude-haiku-4-5").
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn normalize_model(name: &str) -> String {
    if let Some(idx) = name.rfind('-') {
        let suffix = &name[idx + 1..];
        if suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_digit()) {
            return name[..idx].to_string();
        }
    }
    name.to_string()
}

/// The one price lookup every caller uses: the RAW (possibly dated) id first,
/// then its normalized form. `Pricing::lookup` never strips a `-YYYYMMDD`
/// suffix (`normalize_key` only lowercases and de-dots), so the second step is
/// the only thing that prices "claude-opus-5-20260101" against an undated table
/// entry — which is every entry in the built-in snapshot `Pricing::shared()`
/// serves until the async price loader lands.
///
/// The order is load-bearing in both directions: raw first so a dated release
/// priced in its own right wins over its base model, normalized second so a
/// dated id is never left unpriced. Shared by the read paths (`compute_event`,
/// `Agg::add_row`) *and* the archive writer (`rollup::rows_from_events`), whose
/// per-project/branch/account costs are frozen on disk and never recomputed —
/// a one-step lookup there froze a real day's spend at $0 permanently.
pub(crate) fn priced_cost(
    pricing: &Pricing,
    raw: &str,
    input: f64,
    output: f64,
    cc: f64,
    cr: f64,
) -> Option<f64> {
    pricing
        .cost(raw, input, output, cc, cr)
        .or_else(|| pricing.cost(&normalize_model(raw), input, output, cc, cr))
}

/// Last path component of a session cwd → a fallback "project" label. Handles
/// unix and windows separators; empty/blank → "(unknown)".
fn project_of(cwd: &str) -> String {
    let name = cwd.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or("");
    if name.is_empty() {
        "(unknown)".to_string()
    } else {
        name.to_string()
    }
}

/// Resolve the "project" for a session cwd by walking up to the nearest ancestor
/// that contains a `.git` entry (the repo root) and returning its basename, so a
/// session launched in a subdir (…/repo/backend) rolls up to the repo (repo).
/// Falls back to the cwd's own basename when no repo is found or the path is gone.
/// Filesystem-backed, so callers memoize per unique cwd (see `account_events`).
fn resolve_project(cwd: &str) -> String {
    if !cwd.is_empty() {
        let mut dir = std::path::Path::new(cwd);
        loop {
            if dir.join(".git").exists() {
                if let Some(name) = dir.file_name().and_then(|n| n.to_str()) {
                    return name.to_string();
                }
            }
            match dir.parent() {
                Some(p) => dir = p,
                None => break,
            }
        }
    }
    project_of(cwd)
}

fn vendor_of(model: &str) -> &'static str {
    let m = model.to_lowercase();
    if m.contains("claude") {
        "Anthropic"
    } else if m.contains("gpt") || m.contains("o1") || m.contains("o3") || m.contains("codex") {
        "OpenAI"
    } else if m.contains("gemini") {
        "Google"
    } else if m.contains("llama") {
        "Local"
    } else if m.contains("glm") {
        "Zhipu"
    } else if m.contains("deepseek") {
        "DeepSeek"
    } else {
        "Other"
    }
}

// ── period navigation + trend helpers ───────────────────────────────
#[derive(Clone, Copy)]
enum Period {
    Day,
    Week,
    Month,
}

fn parse_period(s: &str) -> Period {
    match s {
        "Day" => Period::Day,
        "Month" => Period::Month,
        _ => Period::Week,
    }
}

fn r2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Total (M tokens, USD cost) over events whose calendar day is in [start, end).
fn sum_range(events: &[Event], start: chrono::NaiveDate, end: chrono::NaiveDate) -> (f64, f64) {
    let mut tok = 0.0;
    let mut cost = 0.0;
    for e in events {
        let d = e.ts.date_naive();
        if d >= start && d < end {
            tok += (e.input + e.cache + e.output) / 1e6;
            cost += e.cost;
        }
    }
    (tok, cost)
}

const WEEKDAY3: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

fn md(d: chrono::NaiveDate) -> String {
    format!("{} {}", MONTHS[(d.month() - 1) as usize], d.day())
}
fn iso(d: chrono::NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}
fn week_start(d: chrono::NaiveDate) -> chrono::NaiveDate {
    d - Duration::days(d.weekday().num_days_from_monday() as i64)
}
fn month_first(y: i32, m: u32) -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(y, m, 1).unwrap()
}
/// The 1st of the month `delta` months from `d`'s month (delta may be negative).
fn add_months(d: chrono::NaiveDate, delta: i32) -> chrono::NaiveDate {
    let total = (d.year() * 12 + d.month() as i32 - 1) + delta;
    month_first(total.div_euclid(12), (total.rem_euclid(12) + 1) as u32)
}

/// Human label of the period containing `reference` (nav-bar title).
fn range_label(kind: Period, reference: DateTime<Local>) -> String {
    let d = reference.date_naive();
    match kind {
        Period::Day => format!(
            "{}, {}",
            WEEKDAY3[d.weekday().num_days_from_monday() as usize],
            md(d)
        ),
        Period::Week => {
            let s = week_start(d);
            format!("{} – {}", md(s), md(s + Duration::days(6)))
        }
        Period::Month => format!("{} {}", MONTHS[(d.month() - 1) as usize], d.year()),
    }
}

/// Zoomed-out trend ending at `reference`'s period: 14 days / 12 weeks / 6
/// months. Points are ordered oldest→newest and carry the anchor date so the UI
/// can click one to view it.
fn build_trend(events: &[Event], kind: Period, reference: DateTime<Local>) -> Vec<TrendPoint> {
    let refd = reference.date_naive();
    match kind {
        Period::Day => (0..14)
            .rev()
            .map(|i| {
                let d = refd - Duration::days(i);
                let (tok, cost) = sum_range(events, d, d + Duration::days(1));
                TrendPoint {
                    label: if i % 3 == 0 { format!("{}", d.day()) } else { String::new() },
                    full: format!("{}, {}", WEEKDAY3[d.weekday().num_days_from_monday() as usize], md(d)),
                    tokens: r2(tok),
                    cost: r2(cost),
                    date: iso(d),
                    current: d == refd,
                }
            })
            .collect(),
        Period::Week => {
            let cur = week_start(refd);
            (0..12)
                .rev()
                .map(|i| {
                    let s = cur - Duration::days(7 * i);
                    let (tok, cost) = sum_range(events, s, s + Duration::days(7));
                    TrendPoint {
                        label: if i % 2 == 0 { md(s) } else { String::new() },
                        full: format!("{} – {}", md(s), md(s + Duration::days(6))),
                        tokens: r2(tok),
                        cost: r2(cost),
                        date: iso(s),
                        current: s == cur,
                    }
                })
                .collect()
        }
        Period::Month => {
            let cur = month_first(refd.year(), refd.month());
            (0..6)
                .rev()
                .map(|i| {
                    let s = add_months(cur, -i);
                    let (tok, cost) = sum_range(events, s, add_months(s, 1));
                    TrendPoint {
                        label: MONTHS[(s.month() - 1) as usize].to_string(),
                        full: format!("{} {}", MONTHS[(s.month() - 1) as usize], s.year()),
                        tokens: r2(tok),
                        cost: r2(cost),
                        date: iso(s),
                        current: s == cur,
                    }
                })
                .collect()
        }
    }
}

/// Build the Day/Week/Month reports + heatmap from a set of already-computed
/// events (config + prices applied). `installed_servers`/`installed_skills` are
/// the whitelist sizes to stamp onto each period (constant across windows).
fn build_reports(
    events: &[Event],
    installed_servers: u64,
    installed_skills: u64,
    now: DateTime<Local>,
) -> Dashboard {
    let today = now.date_naive();
    let mut day = report_day(events, now);
    let mut week = report_week(events, now);
    let mut month = report_month(events, now);
    let heatmap = build_heatmap(events, today);

    // "servers"/"skills" = how many the user has *installed* (global, constant
    // across periods), not how many were called in the window.
    for r in [&mut day, &mut week, &mut month] {
        r.metrics.servers = installed_servers;
        r.metrics.skills = installed_skills;
    }

    // today's displayed tokens (M) for the tray
    let today_tokens: f64 = events
        .iter()
        .filter(|e| e.ts.date_naive() == today)
        .map(|e| (e.input + e.cache + e.output) / 1e6)
        .sum();

    Dashboard {
        day,
        week,
        month,
        heatmap,
        today_tokens,
        generated_at: now.to_rfc3339(),
    }
}

/// Load one account's incremental store (ingest new log bytes, prune, persist),
/// then compute its events with the current config + prices. Returns the events
/// plus the account's installed MCP-server / Skill sets.
fn account_events(
    d: &crate::agents::AgentDescriptor,
    a: &crate::agents::AccountSpec,
    pricing: &Pricing,
    cutoff: i64,
) -> (
    Vec<Event>,
    HashSet<String>,
    HashSet<String>,
    Option<crate::model::QuotaSnapshot>,
    Archive,
) {
    let mut store = Store::load(&a.id);
    let mut dirty = store.ingest(&a.log_root, (d.parser)().as_ref());
    if store.prune_before(cutoff) {
        dirty = true;
    }
    if dirty {
        store.save(&a.id);
    }
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

    // Load unconditionally: callers get the archive back and it must always be
    // valid, whether or not this build had anything new to fold in.
    let mut archive = Archive::load(&a.id);
    // Re-folding every retained event and rewriting the archive costs real time,
    // and this runs on the 30s poll and on every watcher refresh. Skip it when
    // `dirty` says the event set is unchanged: the same events yield the same
    // rows. A price refresh or a whitelist change is NOT a reason to re-absorb —
    // rows keep raw per-model tokens and unfiltered names precisely so both
    // apply at read time in `add_row`. Only the frozen project/branch/account
    // cost can drift, which the archive format already accepts as frozen.
    if dirty || archive.days.is_empty() {
        // Fold this build's live days into the durable archive before the events
        // are mapped. `cutoff` is a timestamp, so its own day is only partly in
        // the store; `absorb` is what keeps that from overwriting a complete row.
        let cutoff_date = DateTime::from_timestamp_millis(cutoff)
            .unwrap_or_default()
            .with_timezone(&Local)
            .date_naive();
        let rows =
            crate::rollup::rows_from_events(&store.events, &mut resolve, &a.label, pricing);
        archive.absorb(rows, cutoff_date);
        archive.save(&a.id);
    }

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
    (events, cfg.mcp_servers, cfg.skills, quota, archive)
}

/// Build a per-account dashboard for every discovered Claude account, plus an
/// aggregate "All" dashboard summing them. Each account has its own incremental
/// store (namespaced cache) and its own MCP/Skill whitelist, so cross-account
/// tool filtering stays correct even in the aggregate.
pub fn build_workspace() -> Workspace {
    let _guard = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let now = Local::now();
    // Reports/heatmap span ~26 weeks (+ prev month); 210 days leaves margin.
    let cutoff = (now - Duration::days(210)).timestamp_millis();
    // Memoized price table (cheap clone); loaded/refreshed off-thread elsewhere
    // so neither parsing nor the network runs while we hold BUILD_LOCK.
    let pricing = Pricing::shared();

    let mut accounts: Vec<AccountData> = Vec::new();
    // Aggregate ("All") is built from every account's computed events; union the
    // whitelists so a server/skill installed in two accounts counts once.
    let mut all_events: Vec<Event> = Vec::new();
    let mut all_servers: HashSet<String> = HashSet::new();
    let mut all_skills: HashSet<String> = HashSet::new();

    for (d, a) in crate::agents::discover_all() {
        let (events, servers, skills, quota, _) = account_events(d, &a, &pricing, cutoff);
        let dash = build_reports(&events, servers.len() as u64, skills.len() as u64, now);
        all_servers.extend(servers);
        all_skills.extend(skills);
        all_events.extend(events);
        accounts.push(AccountData {
            id: a.id,
            label: a.label,
            email: a.email,
            agent: a.agent.to_string(),
            quota,
            dash,
        });
    }

    let all = build_reports(
        &all_events,
        all_servers.len() as u64,
        all_skills.len() as u64,
        now,
    );
    let today_tokens = all.today_tokens;
    Workspace {
        accounts,
        all,
        today_tokens,
    }
}

/// Build a single period report for one account (id) or the "all" aggregate at
/// an arbitrary `reference` datetime (any moment inside the target day/week/
/// month). Powers date navigation and drill-down into past periods.
pub fn build_period(account_id: &str, period: &str, reference: DateTime<Local>) -> PeriodReport {
    let _guard = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cutoff = (Local::now() - Duration::days(210)).timestamp_millis();
    let pricing = Pricing::shared();

    let mut events: Vec<Event> = Vec::new();
    let mut servers: HashSet<String> = HashSet::new();
    let mut skills: HashSet<String> = HashSet::new();
    for (d, a) in crate::agents::discover_all() {
        if account_id != "all" && a.id != account_id {
            continue;
        }
        let (ev, srv, sk, _, _) = account_events(d, &a, &pricing, cutoff);
        events.extend(ev);
        servers.extend(srv);
        skills.extend(sk);
    }

    let mut rep = match parse_period(period) {
        Period::Day => report_day(&events, reference),
        Period::Month => report_month(&events, reference),
        Period::Week => report_week(&events, reference),
    };
    rep.metrics.servers = servers.len() as u64;
    rep.metrics.skills = skills.len() as u64;
    rep
}

/// The aggregate ("All") dashboard across every account — used by the tray
/// label at startup and by the `dump` example. Acquires BUILD_LOCK via
/// build_workspace; must not itself be called while holding it.
pub fn build_dashboard() -> Dashboard {
    build_workspace().all
}

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
    // Not redundant with the BTreeMap's key order. `%m`/`%d` accept unpadded
    // values, so a corrupt key like "2026-1-5" — the same malformed shape
    // `monthly_series` defends against, from a file whose keys `load_from`
    // never validates — parses fine yet sorts *after* "2026-01-06" as a
    // string. The streak walk below reads consecutive dates, so it would
    // silently break a real run in two. Keep this sort.
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
        // Runs ingest, and re-absorbs whenever anything changed, so the archive
        // read below is current either way.
        let (_ev, srv, sk, _q, archive) = account_events(d, &a, &pricing, cutoff);
        let cfg = (d.load_config)(&a);
        for row in archive.days.values() {
            agg.add_row(row, &cfg, &pricing);
        }
        // For the extras (first/last/streak/biggest) and the monthly bars the
        // accounts' days union: a day is active if any account worked that day.
        merge_days(&mut merged, archive);
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

/// The 1..=12 month of a "yyyy-mm…" key, or `None` if it is not one.
fn month_num(key: &str) -> Option<usize> {
    key.get(5..7)
        .and_then(|m| m.parse::<usize>().ok())
        .filter(|m| (1..=12).contains(m))
}

/// Union one account's archive into a cross-account one, summing days the two
/// share.
///
/// PARTIAL by design: only `models` and `hourly` are carried over, because only
/// `all_time_extras`, `monthly_series` and the cross-day hourly fold read the
/// result. Every other `DayRow` field — `projects`, `branches`, `accounts`,
/// `tools`, `mcp`, `skills`, `sessions`, `subagent`, `tool_results`,
/// `tool_errors` — is left at its default here; those come from `Agg::add_row`
/// per account instead. Reading them off the merged archive would silently
/// return zeros.
fn merge_days(into: &mut Archive, from: Archive) {
    for (date, row) in from.days {
        let e = into
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
        // `hourly` is `#[serde(default)]`, so a deserialized row can carry an
        // empty vec. Safe because `e` always comes from `DayRow::new` (24 zeros)
        // and the source side is bounded by `take(24)`.
        for (i, v) in row.hourly.iter().take(24).enumerate() {
            e.hourly[i] += v;
        }
    }
}

/// One bar per calendar month spanned by the archive, oldest→newest, with a
/// sparse axis label so a multi-year range stays readable.
fn monthly_series(archive: &Archive) -> Vec<SeriesPoint> {
    let mut by_month: std::collections::BTreeMap<String, (f64, f64, f64)> =
        std::collections::BTreeMap::new();
    for (date, r) in &archive.days {
        // Keys are ours today ("%Y-%m-%d"), but an `Archive` is deserialized from
        // a file whose keys `load_from` never validates — only its version. A
        // corrupted-but-parseable archive must not panic the whole dashboard
        // build on a slice boundary, so skip a key we cannot read rather than
        // relabel it as January.
        let key = match date.get(..7) {
            Some(k) if month_num(k).is_some() => k.to_string(), // "yyyy-mm"
            _ => continue,
        };
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
            let y = key.get(..4).unwrap_or("");
            // Clamped, not trusted: the collection loop above already rejected a
            // key without a real month, so this only keeps `MONTHS` in bounds.
            let mi = month_num(&key).unwrap_or(1).saturating_sub(1).min(11);
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

/// Derive a computed Event from a stored RawEvent, applying the *current* user
/// config (MCP/Skill whitelist) and prices. This is why these aren't baked into
/// the store: installing an MCP or a price refresh applies retroactively.
fn compute_event(r: &RawEvent, cfg: &UserConfig, pricing: &Pricing) -> Event {
    let ts = DateTime::from_timestamp_millis(r.ts_ms)
        .unwrap_or_default()
        .with_timezone(&Local);
    let model = normalize_model(&r.model);
    // price lookup uses the raw (possibly dated) id, then the normalized one
    let cost_opt = priced_cost(pricing, &r.model, r.in_tok, r.out_tok, r.cc, r.cr);
    let savings = pricing
        .cache_savings(&r.model, r.cr)
        .or_else(|| pricing.cache_savings(&model, r.cr))
        .unwrap_or(0.0);
    let mcp = r
        .mcp
        .iter()
        .filter(|s| cfg.is_user_mcp(s))
        .cloned()
        .collect();
    let skills = r
        .skills
        .iter()
        .filter(|s| cfg.is_user_skill(s))
        .map(|s| s.rsplit(':').next().unwrap_or(s).to_string())
        .collect();
    // Tool-usage breakdown covers built-in tools; mcp__ calls have their own
    // (server-grouped) view, so drop them here to avoid a duplicated, noisier list.
    let tools = r
        .tools
        .iter()
        .filter(|t| !t.starts_with("mcp__"))
        .cloned()
        .collect();
    Event {
        ts,
        session: r.session.clone(),
        model,
        input: r.in_tok,
        cache: r.cc + r.cr,
        output: r.out_tok,
        cost: cost_opt.unwrap_or(0.0),
        savings,
        priced: cost_opt.is_some(),
        project: project_of(&r.cwd),
        branch: r.branch.clone(),
        account: String::new(), // filled in by account_events (knows the account)
        tools,
        sidechain: r.sidechain,
        tool_results: r.tool_results as u64,
        tool_errors: r.tool_errors as u64,
        mcp,
        skills,
    }
}

// ── aggregation helpers ────────────────────────────────────────────
#[derive(Default)]
struct Agg {
    input: f64,
    cache: f64,
    output: f64,
    cost: f64,
    savings: f64,
    subagent_tok: f64, // raw tokens spent inside subagents (isSidechain)
    tool_results: u64,
    tool_errors: u64,
    requests: u64,
    sessions: HashSet<String>,
    /// Sessions contributed by archived rows, which carry a count rather than
    /// the ids. Kept separate from `sessions` so the two are never conflated:
    /// a synthetic id would be indistinguishable from a real one.
    sessions_count: u64,
    mcp_calls: u64,
    skill_calls: u64,
    model_tok: HashMap<String, f64>,
    model_cost: HashMap<String, f64>,
    model_priced: HashMap<String, bool>,
    mcp_counts: HashMap<String, u64>,
    skill_counts: HashMap<String, u64>,
    tool_counts: HashMap<String, u64>,
    project_tok: HashMap<String, f64>,
    project_cost: HashMap<String, f64>,
    branch_tok: HashMap<String, f64>,
    branch_cost: HashMap<String, f64>,
    account_tok: HashMap<String, f64>,
    account_cost: HashMap<String, f64>,
}

impl Agg {
    fn add(&mut self, e: &Event) {
        self.input += e.input;
        self.cache += e.cache;
        self.output += e.output;
        self.cost += e.cost;
        self.savings += e.savings;
        self.tool_results += e.tool_results;
        self.tool_errors += e.tool_errors;
        // An empty model marks an event that is not an LLM request: Claude's
        // slash-command events and Codex's tool/MCP/skill records. Only real API
        // turns may inflate the request count or the model split — a Codex
        // session emits roughly twice as many tool records as turns.
        //
        // `sessions` is gated the same way, and for the same reason: a session
        // that never made a request did no work, so counting it fabricates
        // activity. Concretely, `claude -p "/usage"` writes a whole session log
        // containing only a slash-command line, and without this guard every
        // quota fetch — which Tokenscope itself triggers — added one phantom
        // session to the app's own headline metric.
        if !e.model.is_empty() {
            if !e.session.is_empty() {
                self.sessions.insert(e.session.clone());
            }
            self.requests += 1;
            let tok = e.input + e.cache + e.output;
            // model totals keep all token types so shares sum to Total tokens
            *self.model_tok.entry(e.model.clone()).or_default() += tok;
            *self.model_cost.entry(e.model.clone()).or_default() += e.cost;
            // a model is "priced" if any of its messages had a known price
            *self.model_priced.entry(e.model.clone()).or_default() |= e.priced;
            if e.sidechain {
                self.subagent_tok += tok;
            }
            if !e.project.is_empty() {
                *self.project_tok.entry(e.project.clone()).or_default() += tok;
                *self.project_cost.entry(e.project.clone()).or_default() += e.cost;
            }
            if !e.branch.is_empty() {
                *self.branch_tok.entry(e.branch.clone()).or_default() += tok;
                *self.branch_cost.entry(e.branch.clone()).or_default() += e.cost;
            }
            if !e.account.is_empty() {
                *self.account_tok.entry(e.account.clone()).or_default() += tok;
                *self.account_cost.entry(e.account.clone()).or_default() += e.cost;
            }
        }
        for t in &e.tools {
            *self.tool_counts.entry(t.clone()).or_default() += 1;
        }
        for s in &e.mcp {
            self.mcp_calls += 1;
            *self.mcp_counts.entry(s.clone()).or_default() += 1;
        }
        for s in &e.skills {
            self.skill_calls += 1;
            *self.skill_counts.entry(s.clone()).or_default() += 1;
        }
    }

    /// Fold one archived day into this aggregate.
    ///
    /// This is where the archive's two read-time contracts are honoured: MCP and
    /// skill names were stored unfiltered, so the *current* whitelist applies
    /// here; per-model tokens were stored raw, so the *current* price table
    /// applies here. Both therefore stay retroactive for days the raw event
    /// store can no longer reproduce.
    ///
    /// One deliberate divergence from `add`: a model-less record (a Claude
    /// slash-command line, a Codex tool record) never reaches `r.models`, so its
    /// tokens are invisible here — while `add` books them into `input`/`cache`/
    /// `output` outside its model guard, and `rows_from_events` books them into
    /// the row's `hourly` histogram. All-time `total_tokens` can therefore in
    /// principle undershoot the sum of its own `hourly`. Empirically it is zero
    /// today, because those records carry no tokens; noted so the gap isn't
    /// rediscovered later as a frontend bug.
    fn add_row(&mut self, r: &DayRow, cfg: &UserConfig, pricing: &Pricing) {
        for (raw, b) in &r.models {
            let model = normalize_model(raw);
            let cost = priced_cost(pricing, raw, b.input, b.out, b.cc, b.cr);
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

    fn models(&self) -> Vec<ModelStat> {
        let mut v: Vec<(String, f64, f64)> = self
            .model_tok
            .iter()
            .map(|(k, t)| (k.clone(), *t, *self.model_cost.get(k).unwrap_or(&0.0)))
            .collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        v.into_iter()
            .enumerate()
            .map(|(i, (name, tok, cost))| {
                let priced = *self.model_priced.get(&name).unwrap_or(&false);
                ModelStat {
                    vendor: vendor_of(&name).to_string(),
                    tokens: (tok / 1e6 * 100.0).round() / 100.0,
                    cost: (cost * 100.0).round() / 100.0,
                    color: if i < PALETTE.len() { PALETTE[i] } else { OVERFLOW_GRAY }.to_string(),
                    priced,
                    name,
                }
            })
            .collect()
    }

    fn named(counts: &HashMap<String, u64>) -> Vec<NamedCount> {
        let mut v: Vec<NamedCount> = counts
            .iter()
            .map(|(k, c)| NamedCount {
                name: k.clone(),
                count: *c,
            })
            .collect();
        v.sort_by(|a, b| b.count.cmp(&a.count));
        v
    }

    /// Named token/cost buckets (project / branch), sorted by tokens desc.
    fn named_tokens(
        tok: &HashMap<String, f64>,
        cost: &HashMap<String, f64>,
    ) -> Vec<NamedTokens> {
        let mut v: Vec<NamedTokens> = tok
            .iter()
            .map(|(k, t)| NamedTokens {
                name: k.clone(),
                tokens: (t / 1e6 * 100.0).round() / 100.0,
                cost: (cost.get(k).copied().unwrap_or(0.0) * 100.0).round() / 100.0,
            })
            .collect();
        v.sort_by(|a, b| {
            b.tokens
                .partial_cmp(&a.tokens)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        v
    }

    fn metrics(&self, delta_tokens: f64, delta_cost: f64) -> Metrics {
        Metrics {
            total_tokens: ((self.input + self.cache + self.output) / 1e6 * 100.0).round() / 100.0,
            input_tokens: (self.input / 1e6 * 100.0).round() / 100.0,
            cache_tokens: (self.cache / 1e6 * 100.0).round() / 100.0,
            output_tokens: (self.output / 1e6 * 100.0).round() / 100.0,
            cost: (self.cost * 100.0).round() / 100.0,
            cache_savings: (self.savings * 100.0).round() / 100.0,
            subagent_tokens: (self.subagent_tok / 1e6 * 100.0).round() / 100.0,
            tool_results: self.tool_results,
            tool_errors: self.tool_errors,
            mcp_calls: self.mcp_calls,
            skill_calls: self.skill_calls,
            requests: self.requests,
            sessions: self.sessions.len() as u64 + self.sessions_count,
            delta_tokens,
            delta_cost,
            servers: self.mcp_counts.len() as u64,
            skills: self.skill_counts.len() as u64,
        }
    }
}

/// Percentage change of `cur` vs `prev`, e.g. +20.0 for a 20% increase,
/// rounded to 2 decimals. Returns 0 when there's no baseline to compare.
fn pct_delta(cur: f64, prev: f64) -> f64 {
    if prev <= 0.0 {
        return 0.0;
    }
    ((cur - prev) / prev * 10000.0).round() / 100.0
}

// ── Day report: today, 24 hourly buckets ───────────────────────────
fn report_day(events: &[Event], now: DateTime<Local>) -> PeriodReport {
    let today = now.date_naive();
    let yesterday = today - Duration::days(1);
    let mut agg = Agg::default();
    let mut prev = Agg::default();
    let mut buckets = vec![(0.0f64, 0.0f64, 0.0f64); 24]; // (input, cache, output) M
    let mut req_b = vec![0.0f64; 24];
    let mut cost_b = vec![0.0f64; 24];
    let mut hour = vec![0.0f64; 24];

    for e in events {
        let d = e.ts.date_naive();
        if d == today {
            agg.add(e);
            let h = e.ts.hour() as usize;
            hour[h] += (e.input + e.cache + e.output) / 1e6;
            buckets[h].0 += e.input / 1e6;
            buckets[h].1 += e.cache / 1e6;
            buckets[h].2 += e.output / 1e6;
            // Match Agg::add exactly: only the request COUNT excludes model-less
            // (slash-command) events; total cost accumulates unconditionally
            // (those events carry cost 0, so this is identical today).
            if !e.model.is_empty() {
                req_b[h] += 1.0;
            }
            cost_b[h] += e.cost;
        } else if d == yesterday {
            prev.add(e);
        }
    }

    let series = (0..24)
        .map(|h| SeriesPoint {
            // axis ticks every 4h, skipping the 00/24 endpoints
            label: if h % 4 == 0 && h != 0 {
                format!("{:02}", h)
            } else {
                String::new()
            },
            full: format!("{:02}:00", h),
            input: buckets[h].0,
            cache: buckets[h].1,
            output: buckets[h].2,
            date: String::new(), // an hour isn't a drillable calendar day
        })
        .collect();

    PeriodReport {
        metrics: agg.metrics(
            pct_delta(
                agg.input + agg.cache + agg.output,
                prev.input + prev.cache + prev.output,
            ),
            pct_delta(agg.cost, prev.cost),
        ),
        series,
        models: agg.models(),
        projects: Agg::named_tokens(&agg.project_tok, &agg.project_cost),
        branches: Agg::named_tokens(&agg.branch_tok, &agg.branch_cost),
        accounts: Agg::named_tokens(&agg.account_tok, &agg.account_cost),
        tools: Agg::named(&agg.tool_counts),
        mcp: Agg::named(&agg.mcp_counts),
        skills: Agg::named(&agg.skill_counts),
        req_trend: req_b,
        cost_trend: cost_b,
        hourly: hour,
        range: range_label(Period::Day, now),
        trend: build_trend(events, Period::Day, now),
    }
}

// ── Week report: current calendar week (Mon-Sun) vs previous week ────
fn report_week(events: &[Event], now: DateTime<Local>) -> PeriodReport {
    let today = now.date_naive();
    // Monday of the current week (Mon=0 … Sun=6).
    let start = today - Duration::days(today.weekday().num_days_from_monday() as i64);
    let next_start = start + Duration::days(7);
    let prev_start = start - Duration::days(7);

    let mut agg = Agg::default();
    let mut prev = Agg::default();
    let mut buckets = vec![(0.0f64, 0.0f64, 0.0f64); 7];
    let mut req_b = vec![0.0f64; 7];
    let mut cost_b = vec![0.0f64; 7];
    let mut hour = vec![0.0f64; 24];

    for e in events {
        let d = e.ts.date_naive();
        if d >= start && d < next_start {
            agg.add(e);
            hour[e.ts.hour() as usize] += (e.input + e.cache + e.output) / 1e6;
            let idx = (d - start).num_days() as usize;
            if idx < buckets.len() {
                buckets[idx].0 += e.input / 1e6;
                buckets[idx].1 += e.cache / 1e6;
                buckets[idx].2 += e.output / 1e6;
                // Match Agg::add: only the request COUNT excludes model-less
                // events; cost accumulates unconditionally (their cost is 0).
                if !e.model.is_empty() {
                    req_b[idx] += 1.0;
                }
                cost_b[idx] += e.cost;
            }
        } else if d >= prev_start && d < start {
            prev.add(e);
        }
    }

    let weekday = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let series = (0..7usize)
        .map(|i| {
            let date = start + Duration::days(i as i64);
            let wd = weekday[i];
            SeriesPoint {
                label: wd.to_string(),
                full: format!("{} {} {}", wd, MONTHS[(date.month() - 1) as usize], date.day()),
                input: buckets[i].0,
                cache: buckets[i].1,
                output: buckets[i].2,
                date: iso(date),
            }
        })
        .collect();

    PeriodReport {
        metrics: agg.metrics(
            pct_delta(
                agg.input + agg.cache + agg.output,
                prev.input + prev.cache + prev.output,
            ),
            pct_delta(agg.cost, prev.cost),
        ),
        series,
        models: agg.models(),
        projects: Agg::named_tokens(&agg.project_tok, &agg.project_cost),
        branches: Agg::named_tokens(&agg.branch_tok, &agg.branch_cost),
        accounts: Agg::named_tokens(&agg.account_tok, &agg.account_cost),
        tools: Agg::named(&agg.tool_counts),
        mcp: Agg::named(&agg.mcp_counts),
        skills: Agg::named(&agg.skill_counts),
        req_trend: req_b,
        cost_trend: cost_b,
        hourly: hour,
        range: range_label(Period::Week, now),
        trend: build_trend(events, Period::Week, now),
    }
}

// ── Month report: current calendar month vs previous calendar month ──
fn report_month(events: &[Event], now: DateTime<Local>) -> PeriodReport {
    use chrono::NaiveDate;
    let today = now.date_naive();
    let (y, m) = (today.year(), today.month());
    let cur_first = NaiveDate::from_ymd_opt(y, m, 1).unwrap();
    let next_first = if m == 12 {
        NaiveDate::from_ymd_opt(y + 1, 1, 1).unwrap()
    } else {
        NaiveDate::from_ymd_opt(y, m + 1, 1).unwrap()
    };
    let (py, pm) = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
    let prev_first = NaiveDate::from_ymd_opt(py, pm, 1).unwrap();
    let days_in_month = (next_first - cur_first).num_days() as usize;

    let mut agg = Agg::default();
    let mut prev = Agg::default();
    let mut buckets = vec![(0.0f64, 0.0f64, 0.0f64); days_in_month];
    let mut req_b = vec![0.0f64; days_in_month];
    let mut cost_b = vec![0.0f64; days_in_month];
    let mut hour = vec![0.0f64; 24];

    for e in events {
        let d = e.ts.date_naive();
        if d >= cur_first && d < next_first {
            agg.add(e);
            hour[e.ts.hour() as usize] += (e.input + e.cache + e.output) / 1e6;
            let idx = (d - cur_first).num_days() as usize;
            if idx < buckets.len() {
                buckets[idx].0 += e.input / 1e6;
                buckets[idx].1 += e.cache / 1e6;
                buckets[idx].2 += e.output / 1e6;
                // Match Agg::add: only the request COUNT excludes model-less
                // events; cost accumulates unconditionally (their cost is 0).
                if !e.model.is_empty() {
                    req_b[idx] += 1.0;
                }
                cost_b[idx] += e.cost;
            }
        } else if d >= prev_first && d < cur_first {
            prev.add(e);
        }
    }

    let series = (0..days_in_month)
        .map(|i| {
            let dn = (i + 1) as u32;
            let label = if i == 0 || dn % 5 == 0 {
                dn.to_string()
            } else {
                String::new()
            };
            SeriesPoint {
                label,
                full: format!("{} {}", MONTHS[(m - 1) as usize], dn),
                input: buckets[i].0,
                cache: buckets[i].1,
                output: buckets[i].2,
                date: iso(cur_first + Duration::days(i as i64)),
            }
        })
        .collect();

    PeriodReport {
        metrics: agg.metrics(
            pct_delta(
                agg.input + agg.cache + agg.output,
                prev.input + prev.cache + prev.output,
            ),
            pct_delta(agg.cost, prev.cost),
        ),
        series,
        models: agg.models(),
        projects: Agg::named_tokens(&agg.project_tok, &agg.project_cost),
        branches: Agg::named_tokens(&agg.branch_tok, &agg.branch_cost),
        accounts: Agg::named_tokens(&agg.account_tok, &agg.account_cost),
        tools: Agg::named(&agg.tool_counts),
        mcp: Agg::named(&agg.mcp_counts),
        skills: Agg::named(&agg.skill_counts),
        req_trend: req_b,
        cost_trend: cost_b,
        hourly: hour,
        range: range_label(Period::Month, now),
        trend: build_trend(events, Period::Month, now),
    }
}

// ── Heatmap: last ~26 weeks daily totals ────────────────────────────
fn build_heatmap(events: &[Event], today: chrono::NaiveDate) -> Vec<HeatDay> {
    let start = today - Duration::days(25 * 7 + today.weekday().num_days_from_sunday() as i64);
    let mut by_day: HashMap<chrono::NaiveDate, f64> = HashMap::new();
    for e in events {
        let d = e.ts.date_naive();
        if d >= start && d <= today {
            *by_day.entry(d).or_default() += (e.input + e.cache + e.output) / 1e6;
        }
    }
    let mut days = Vec::new();
    let mut d = start;
    let mut maxv = 0.0f64;
    while d <= today {
        let t = *by_day.get(&d).unwrap_or(&0.0);
        maxv = maxv.max(t);
        days.push((d, t));
        d += Duration::days(1);
    }
    days.into_iter()
        .map(|(date, tokens)| {
            let f = if maxv > 0.0 { tokens / maxv } else { 0.0 };
            let level = if tokens == 0.0 {
                0
            } else if f < 0.25 {
                1
            } else if f < 0.5 {
                2
            } else if f < 0.75 {
                3
            } else {
                4
            };
            HeatDay {
                date: date.format("%Y-%m-%d").to_string(),
                tokens: (tokens * 100.0).round() / 100.0,
                level,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare Event for Agg tests. An empty `model` is the store's marker for a
    /// record that is not an LLM request (Claude's slash-command lines, Codex's
    /// tool/MCP/skill records).
    fn ev(session: &str, model: &str) -> Event {
        Event {
            ts: Local::now(),
            session: session.to_string(),
            model: model.to_string(),
            input: 0.0,
            cache: 0.0,
            output: 0.0,
            cost: 0.0,
            savings: 0.0,
            priced: false,
            project: String::new(),
            branch: String::new(),
            account: String::new(),
            tools: Vec::new(),
            sidechain: false,
            tool_results: 0,
            tool_errors: 0,
            mcp: Vec::new(),
            skills: Vec::new(),
        }
    }

    #[test]
    fn a_session_that_made_no_request_is_not_counted() {
        // `claude -p "/usage"` writes a whole session log whose only content is
        // the slash-command line: a fresh sessionId, no assistant message, no
        // model. Counting it lets a quota fetch inflate the app's own metric.
        let mut agg = Agg::default();
        agg.add(&ev("usage-poll-session", ""));
        assert_eq!(agg.sessions.len(), 0);
        assert_eq!(agg.requests, 0);
    }

    #[test]
    fn a_session_counts_once_a_real_request_lands_in_it() {
        let mut agg = Agg::default();
        agg.add(&ev("real-session", "")); // a slash command…
        agg.add(&ev("real-session", "claude-opus-5")); // …then actual work
        agg.add(&ev("real-session", "claude-opus-5"));
        assert_eq!(agg.sessions.len(), 1);
        assert_eq!(agg.requests, 2);
    }

    #[test]
    fn codex_models_are_attributed_to_openai() {
        assert_eq!(vendor_of("gpt-5.6-sol"), "OpenAI");
        // Has no "gpt" in the name, so it needs its own rule or it lands in "Other".
        assert_eq!(vendor_of("codex-auto-review"), "OpenAI");
        // Unchanged for the models already handled.
        assert_eq!(vendor_of("claude-opus-5"), "Anthropic");
    }

    use crate::pricing::ModelPrice;
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
        r.projects.insert("proj-a".to_string(), (1.5, 2.25));
        r.branches.insert("main".to_string(), (2.0, 3.0));
        r.accounts.insert("acct-1".to_string(), (0.5, 0.75));
        r.subagent = 42.0;
        r.tool_results = 7;
        r.tool_errors = 1;
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

    #[test]
    fn an_archived_row_is_priced_by_its_raw_id_before_its_normalized_one() {
        // Archived rows key `models` by the raw (possibly dated) id specifically
        // so a price update or a dated release's own rate applies retroactively.
        // Put different prices on the raw id and its normalized form: if
        // `add_row` looked up the normalized id first, this row would be priced
        // at 7.0 (1_000_000 * 5e-6 + 100_000 * 20e-6) instead of the 3.0 below.
        let pricing = Pricing::with_exact(&[
            (
                "claude-opus-5-20260101",
                ModelPrice { input: 2e-6, output: 10e-6, cache_create: 0.0, cache_read: 0.0 },
            ),
            (
                "claude-opus-5",
                ModelPrice { input: 5e-6, output: 20e-6, cache_create: 0.0, cache_read: 0.0 },
            ),
        ]);
        let mut agg = Agg::default();
        agg.add_row(&day_row(), &cfg_with(&[], &[]), &pricing);

        assert_eq!(agg.cost, 3.0);
        assert_eq!(agg.model_priced.get("claude-opus-5"), Some(&true));
    }

    #[test]
    fn an_archived_rows_project_branch_account_and_passthrough_fields_convert_correctly() {
        // `DayRow` stores project/branch/account tokens in M tokens (to match
        // how they're frozen at archive time); `Agg` accumulates raw tokens
        // everywhere else. This pins the `* 1e6` conversion in `add_row`: a
        // dropped or inverted conversion here would be a 1,000,000x error that
        // every other test in this module is blind to.
        let mut agg = Agg::default();
        agg.add_row(&day_row(), &cfg_with(&[], &[]), &Pricing::empty());

        assert_eq!(agg.project_tok.get("proj-a"), Some(&1_500_000.0));
        assert_eq!(agg.branch_tok.get("main"), Some(&2_000_000.0));
        assert_eq!(agg.account_tok.get("acct-1"), Some(&500_000.0));
        // Costs are frozen USD already, not M tokens — they pass through unchanged.
        assert_eq!(agg.project_cost.get("proj-a"), Some(&2.25));
        assert_eq!(agg.branch_cost.get("main"), Some(&3.0));
        assert_eq!(agg.account_cost.get("acct-1"), Some(&0.75));
        // Plain counters/totals pass through unchanged too.
        assert_eq!(agg.subagent_tok, 42.0);
        assert_eq!(agg.tool_results, 7);
        assert_eq!(agg.tool_errors, 1);
    }

    /// An archive of `(date, input, cc, cr, out)` rows, one model each.
    fn archive_of(rows: &[(&str, f64, f64, f64, f64)]) -> Archive {
        let mut archive = Archive::default();
        for (date, input, cc, cr, out) in rows {
            let mut r = DayRow::new(date);
            r.models.insert(
                "claude-opus-5".to_string(),
                TokBits { input: *input, cc: *cc, cr: *cr, out: *out, requests: 1 },
            );
            archive.days.insert(date.to_string(), r);
        }
        archive
    }

    #[test]
    fn all_time_extras_describe_the_archived_range() {
        // Raw token counts, in the millions: `all_time_extras` reports M tokens
        // rounded to 2 dp (the `r2` Global Constraint), so toy values would all
        // round to 0.0 and the assertion below would prove nothing.
        //
        // The biggest day carries most of its tokens in `input`/`cc`/`cr` and the
        // *fewest* in `out`, so an implementation that summed only `out` would
        // both misreport its size (20.0, not 90.0) and crown the 9th instead.
        let archive = archive_of(&[
            ("2026-01-05", 0.0, 0.0, 0.0, 10e6),
            ("2026-01-06", 40e6, 20e6, 10e6, 20e6), // 90M, the biggest day
            ("2026-01-07", 0.0, 0.0, 0.0, 20e6),
            // A row with no tokens at all — the archive holds one for a day that
            // saw only slash-command records. It sits in the gap between the
            // streak and the last day, so if the `tok <= 0.0` guard were dropped
            // it would both count as active (5) and bridge the streak (05..09).
            ("2026-01-08", 0.0, 0.0, 0.0, 0.0),
            ("2026-01-09", 0.0, 0.0, 0.0, 30e6),
        ]);

        let x = all_time_extras(&archive);
        assert_eq!(x.first, "2026-01-05");
        assert_eq!(x.last, "2026-01-09");
        assert_eq!(x.active_days, 4);
        assert_eq!(x.longest_streak, 3); // 05, 06, 07
        assert_eq!(x.biggest_day, Some(("2026-01-06".to_string(), 90.0)));
    }

    #[test]
    fn merging_two_accounts_sums_the_days_they_share_and_keeps_the_ones_they_dont() {
        let mut merged = Archive::default();
        let mut a = archive_of(&[("2026-01-05", 1e6, 2e6, 3e6, 4e6)]);
        let mut b = archive_of(&[
            ("2026-01-05", 10e6, 20e6, 30e6, 40e6),
            ("2026-01-06", 5e6, 0.0, 0.0, 0.0), // only this account worked the 6th
        ]);
        a.days.get_mut("2026-01-05").unwrap().hourly[3] = 1.5;
        b.days.get_mut("2026-01-05").unwrap().hourly[3] = 2.5;
        b.days.get_mut("2026-01-05").unwrap().hourly[9] = 7.0;

        merge_days(&mut merged, a);
        merge_days(&mut merged, b);

        assert_eq!(merged.days.len(), 2);
        let shared = &merged.days["2026-01-05"].models["claude-opus-5"];
        assert_eq!(shared.input, 11e6);
        assert_eq!(shared.cc, 22e6);
        assert_eq!(shared.cr, 33e6);
        assert_eq!(shared.out, 44e6);
        assert_eq!(shared.requests, 2);
        // Histograms add bucket-wise, and untouched buckets stay zero.
        let h = &merged.days["2026-01-05"].hourly;
        assert_eq!(h.len(), 24);
        assert_eq!(h[3], 4.0);
        assert_eq!(h[9], 7.0);
        assert_eq!(h[0], 0.0);
        // A day only one account has survives untouched.
        assert_eq!(merged.days["2026-01-06"].models["claude-opus-5"].input, 5e6);
    }

    #[test]
    fn merging_a_row_whose_hourly_vector_is_empty_does_not_panic() {
        // `DayRow::hourly` is `#[serde(default)]`, so a row read back from disk
        // can legitimately have no buckets at all. The merge survives it only
        // because the destination always comes from `DayRow::new` (24 zeros) and
        // the source is bounded by `take(24)`.
        let mut merged = Archive::default();
        let mut incoming = archive_of(&[("2026-01-05", 1e6, 0.0, 0.0, 2e6)]);
        incoming.days.get_mut("2026-01-05").unwrap().hourly = Vec::new();

        merge_days(&mut merged, incoming);

        let day = &merged.days["2026-01-05"];
        assert_eq!(day.hourly.len(), 24);
        assert!(day.hourly.iter().all(|v| *v == 0.0));
        assert_eq!(day.models["claude-opus-5"].input, 1e6);
        assert_eq!(day.models["claude-opus-5"].out, 2e6);
    }

    #[test]
    fn monthly_series_buckets_days_into_months_in_m_tokens() {
        // Two days in January, one in February. Cache is creation + read.
        let series = monthly_series(&archive_of(&[
            ("2026-01-05", 1e6, 2e6, 3e6, 4e6),
            ("2026-01-20", 1e6, 0.0, 0.0, 1e6),
            ("2026-02-03", 6e6, 1e6, 1e6, 2e6),
        ]));

        assert_eq!(series.len(), 2);
        assert_eq!(series[0].input, 2.0);
        assert_eq!(series[0].cache, 5.0);
        assert_eq!(series[0].output, 5.0);
        assert_eq!(series[1].input, 6.0);
        assert_eq!(series[1].cache, 2.0);
        assert_eq!(series[1].output, 2.0);
        // The drill-down anchor is the first of the month, not a day that exists.
        assert_eq!(series[0].date, "2026-01-01");
        assert_eq!(series[1].date, "2026-02-01");
        assert_eq!(series[0].full, "Jan 2026");
        assert_eq!(series[1].full, "Feb 2026");
        // Six or fewer months: every bar is labelled.
        assert_eq!(series[0].label, "Jan");
        assert_eq!(series[1].label, "Feb");
    }

    #[test]
    fn a_long_monthly_range_labels_only_about_six_ticks() {
        // 13 months → every 2nd bar labelled, so the axis stays readable instead
        // of printing a tick per month.
        let rows: Vec<(String, f64, f64, f64, f64)> = (0..13)
            .map(|i| {
                let (y, m) = (2025 + i / 12, i % 12 + 1);
                (format!("{y}-{m:02}-05"), 1e6, 0.0, 0.0, 0.0)
            })
            .collect();
        let refs: Vec<(&str, f64, f64, f64, f64)> =
            rows.iter().map(|(d, a, b, c, e)| (d.as_str(), *a, *b, *c, *e)).collect();

        let series = monthly_series(&archive_of(&refs));

        assert_eq!(series.len(), 13);
        let labelled: Vec<usize> = series
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.label.is_empty())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(labelled, vec![0, 2, 4, 6, 8, 10, 12]);
        // Every bar still carries its full label for the tooltip.
        assert_eq!(series[1].full, "Feb 2025");
        assert_eq!(series[12].full, "Jan 2026");
    }

    #[test]
    fn a_malformed_archive_key_is_skipped_rather_than_crashing_the_build() {
        // `Archive::load_from` validates the document version and nothing else,
        // so a corrupted-but-parseable file can hand us any key at all. None of
        // these may panic on a slice boundary or an out-of-range month index.
        let mut archive = archive_of(&[("2026-01-05", 1e6, 0.0, 0.0, 0.0)]);
        for bad in ["", "2026", "2026-00-01", "2026-13-01", "2026-xx-01", "20é6-01-01"] {
            archive.days.insert(bad.to_string(), DayRow::new(bad));
        }

        let series = monthly_series(&archive);

        // Only the one well-formed day produced a bar.
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].full, "Jan 2026");
        assert_eq!(series[0].date, "2026-01-01");
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
}
