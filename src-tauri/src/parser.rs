// Parse ~/.claude/projects/**/*.jsonl, dedupe assistant messages by id,
// classify tool calls (user-installed MCP / Skill only), and aggregate
// into Day / Week / Month reports + a daily heatmap.
use crate::config::UserConfig;
use crate::model::*;
use crate::pricing::Pricing;
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
    // Resolve each event's project to its git-repo root, memoized per unique cwd
    // so the (filesystem-backed) walk-up runs once per directory, not per event.
    let mut proj_memo: HashMap<String, String> = HashMap::new();
    let events = store
        .events
        .iter()
        .map(|r| {
            let mut e = compute_event(r, &cfg, pricing);
            e.project = proj_memo
                .entry(r.cwd.clone())
                .or_insert_with(|| resolve_project(&r.cwd))
                .clone();
            e.account = a.label.clone();
            e
        })
        .collect();
    (events, cfg.mcp_servers, cfg.skills)
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
        let (events, servers, skills) = account_events(d, &a, &pricing, cutoff);
        let dash = build_reports(&events, servers.len() as u64, skills.len() as u64, now);
        all_servers.extend(servers);
        all_skills.extend(skills);
        all_events.extend(events);
        accounts.push(AccountData {
            id: a.id,
            label: a.label,
            email: a.email,
            agent: a.agent.to_string(),
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
        let (ev, srv, sk) = account_events(d, &a, &pricing, cutoff);
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

/// Derive a computed Event from a stored RawEvent, applying the *current* user
/// config (MCP/Skill whitelist) and prices. This is why these aren't baked into
/// the store: installing an MCP or a price refresh applies retroactively.
fn compute_event(r: &RawEvent, cfg: &UserConfig, pricing: &Pricing) -> Event {
    let ts = DateTime::from_timestamp_millis(r.ts_ms)
        .unwrap_or_default()
        .with_timezone(&Local);
    let model = normalize_model(&r.model);
    // price lookup uses the raw (possibly dated) id, then the normalized one
    let cost_opt = pricing
        .cost(&r.model, r.in_tok, r.out_tok, r.cc, r.cr)
        .or_else(|| pricing.cost(&model, r.in_tok, r.out_tok, r.cc, r.cr));
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
        if !e.session.is_empty() {
            self.sessions.insert(e.session.clone());
        }
        // An empty model marks an event that is not an LLM request: Claude's
        // slash-command events and Codex's tool/MCP/skill records. Only real API
        // turns may inflate the request count or the model split — a Codex
        // session emits roughly twice as many tool records as turns.
        if !e.model.is_empty() {
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
            sessions: self.sessions.len() as u64,
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

    #[test]
    fn codex_models_are_attributed_to_openai() {
        assert_eq!(vendor_of("gpt-5.6-sol"), "OpenAI");
        // Has no "gpt" in the name, so it needs its own rule or it lands in "Other".
        assert_eq!(vendor_of("codex-auto-review"), "OpenAI");
        // Unchanged for the models already handled.
        assert_eq!(vendor_of("claude-opus-5"), "Anthropic");
    }
}
