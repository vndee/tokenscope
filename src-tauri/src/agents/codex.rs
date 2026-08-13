// Codex CLI adapter. Logs live at <codex-dir>/sessions/YYYY/MM/DD/rollout-*.jsonl,
// one session per file. See docs/superpowers/specs/2026-08-12-codex-tracking-design.md.
use super::{AccountSpec, AgentDescriptor, FileState, LogParser};
use crate::config::{self, UserConfig};
use crate::model::{QuotaSnapshot, QuotaWindow};
use crate::store::RawEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;

pub const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "codex",
    display: "Codex",
    discover,
    load_config,
    parser,
};

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
// `#[serde(default)]` lets a partial or older persisted payload (missing a
// field serde would otherwise require) deserialize field-by-field instead of
// failing wholesale — a wholesale failure loses `prev` and re-adds an entire
// session's tokens on the next incremental pass.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
struct Carry {
    session: String,
    model: String,
    cwd: String,
    branch: String,
    sidechain: bool,
    prev: Option<Cum>,
    /// Set while the replayed parent transcript at the top of a forked rollout
    /// is being skipped: the UUIDv7 mint time (ms) of *this* thread's own id.
    /// `None` in a normal file, and cleared for good at the first own-era turn.
    /// Carried because an incremental read can resume mid-replay.
    replay_until: Option<u64>,
    /// Latched once a fork marker has armed the skip in this file. A nested
    /// fork's *replayed* session_meta carries a `forked_from_id` of its own;
    /// without this latch it would re-arm the skip mid-file and swallow the
    /// fork's real usage.
    replay_armed: bool,
    /// Newest rate_limits seen in this file: (event ts_ms, snapshot).
    quota: Option<(i64, QuotaSnapshot)>,
}

pub(super) struct CodexParser;

struct CodexState {
    c: Carry,
    /// Skills already counted in the current turn, so re-reading a SKILL.md
    /// mid-turn doesn't inflate the count. Deliberately not carried across an
    /// incremental read: turn boundaries reset it anyway.
    turn_skills: HashSet<String>,
}

impl LogParser for CodexParser {
    fn new_file_state(&self, carry: Option<&Value>) -> Box<dyn FileState> {
        let c = carry
            .and_then(|v| serde_json::from_value::<Carry>(v.clone()).ok())
            .unwrap_or_default();
        Box::new(CodexState {
            c,
            turn_skills: HashSet::new(),
        })
    }
}

/// Every `skills/<name>/SKILL.md` or `skills/<plugin>/<name>/SKILL.md` path in
/// a shell command, in order. A plugin-scoped skill is labelled `plugin:name`,
/// matching the `input.skill` values Claude's parser already emits for its own
/// plugin-scoped skills, so the two agents' Skill breakdowns line up.
///
/// Codex has no skill tool call: a skill is invoked by reading its SKILL.md, so
/// that read is the signal. Requiring an exact `/SKILL.md` tail (at one or two
/// path levels) keeps out reference files under a skill
/// (`skills/using-superpowers/references/codex-tools.md`, no `SKILL.md` tail
/// at either level). A path whose segment right after `skills/` starts with
/// `.` is excluded outright — that's how nested system paths
/// (`skills/.system/openai-docs/SKILL.md`) stay out even though they'd
/// otherwise match the two-level shape. One command can open several skills.
fn skill_names(cmd: &str) -> Vec<String> {
    const MARK: &str = "skills/";
    let mut out = Vec::new();
    let mut rest = cmd;
    while let Some(i) = rest.find(MARK) {
        rest = &rest[i + MARK.len()..];
        let mut segs = rest.split('/');
        let Some(seg1) = segs.next() else {
            continue;
        };
        if seg1.is_empty() || seg1.starts_with('.') {
            continue;
        }
        if rest[seg1.len()..].starts_with("/SKILL.md") {
            let n = seg1.to_string();
            if !out.contains(&n) {
                out.push(n);
            }
            continue;
        }
        if let Some(seg2) = segs.next() {
            let prefix_len = seg1.len() + 1 + seg2.len();
            if !seg2.is_empty() && rest[prefix_len..].starts_with("/SKILL.md") {
                let n = format!("{seg1}:{seg2}");
                if !out.contains(&n) {
                    out.push(n);
                }
            }
        }
    }
    out
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

/// Milliseconds encoded in the leading 48 bits of a UUIDv7. Codex mints thread
/// ids and turn ids as UUIDv7, so comparing two of these orders them by mint
/// time — which is how a forked rollout's replayed (parent-era) turns are told
/// apart from its own. `None` for anything that isn't a readable UUID.
fn uuid7_ms(id: &str) -> Option<u64> {
    let hex: String = id.chars().filter(|c| *c != '-').take(12).collect();
    if hex.len() != 12 {
        return None;
    }
    u64::from_str_radix(&hex, 16).ok()
}

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

impl CodexState {
    /// A RawEvent pre-filled with this session's attribution, carrying no usage.
    /// Tool/skill events reuse it; `id` stays empty because the byte-offset
    /// manifest already guarantees each line is read exactly once.
    ///
    /// `model` is deliberately left empty, which is the store's existing marker
    /// for "not an LLM request" (Claude's slash-command events use it the same
    /// way). A tool, MCP or skill record is not an API turn — only a
    /// `token_count` is — and filling the model in here made every one of them
    /// count as a request. `on_token_count` sets the model on the events that
    /// really are turns.
    fn base(&self, ts_ms: i64) -> RawEvent {
        RawEvent {
            ts_ms,
            session: self.c.session.clone(),
            model: String::new(),
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
        // session's source is a plain string ("vscode"). Latch, don't assign:
        // real logs replay session_meta (a second record with a plain "vscode"
        // source, observed ~3ms after the first in subagent files) and an
        // unconditional assignment would flip sidechain back to false on that
        // replay, misattributing the whole session's tokens to the main loop.
        if p
            .get("source")
            .and_then(|v| v.as_object())
            .map(|o| o.contains_key("subagent"))
            .unwrap_or(false)
        {
            self.c.sidechain = true;
        }
        // A forked thread's rollout does not start empty: it opens by replaying
        // the whole transcript of the thread it forked from — session_meta,
        // task_started, turn_context, token_count and mcp_tool_call_end alike —
        // restamped at the fork instant but carrying the parent's cumulative
        // counters verbatim. Those records are the parent's and are already
        // counted in the parent's own file, so here they may only establish this
        // file's baseline. `forked_from_id` is the fork's own declaration that a
        // replay follows; `id` is its own thread id, which dates the fork.
        //
        // A file with no `forked_from_id` is never touched by this, so an
        // ordinary session (and a spawned sub-agent, which starts clean) is
        // parsed exactly as before.
        if !self.c.replay_armed && p.get("forked_from_id").is_some() {
            if let Some(own) = p.get("id").and_then(|v| v.as_str()).and_then(uuid7_ms) {
                self.c.replay_until = Some(own);
                self.c.replay_armed = true;
            }
        }
    }

    /// A turn boundary (`task_started` / `turn_context`). Turn ids are UUIDv7,
    /// so a turn minted before this thread's own id existed cannot be this
    /// thread's work — it belongs to the replayed parent transcript. The first
    /// turn minted at or after the fork ends the replay for good (the marker is
    /// cleared, never re-armed), which is also why the skipped region is exactly
    /// the file's leading prefix. A turn id we cannot read ends the replay too:
    /// counting a little twice is a smaller error than discarding real usage.
    /// A record with no turn id at all says nothing about the boundary and
    /// leaves the state untouched.
    fn on_turn(&mut self, turn_id: Option<&str>) {
        let Some(own) = self.c.replay_until else {
            return;
        };
        let Some(id) = turn_id else {
            return;
        };
        if uuid7_ms(id).map(|t| t >= own).unwrap_or(true) {
            self.c.replay_until = None;
        }
    }

    /// Are we inside the replayed parent prefix of a forked rollout?
    fn replaying(&self) -> bool {
        self.c.replay_until.is_some()
    }

    fn on_token_count(&mut self, p: &Value, ts_ms: i64) -> Option<RawEvent> {
        if let Some(rl) = p.get("rate_limits").filter(|v| !v.is_null()) {
            if let Some(q) = quota_from_rate_limits(rl, ts_ms) {
                if self.c.quota.as_ref().map(|(prev, _)| ts_ms >= *prev).unwrap_or(true) {
                    self.c.quota = Some((ts_ms, q));
                }
            }
        }

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
        // This one *is* an API turn, so it carries the model — both for pricing
        // and as the request-count marker (see `base`). A turn whose model we
        // never saw is recorded as "unknown" rather than dropped.
        e.model = if self.c.model.is_empty() {
            "unknown".into()
        } else {
            self.c.model.clone()
        };
        e.in_tok = uncached;
        e.cr = d_cached;
        e.cc = d_cw;
        e.out_tok = d_out;
        Some(e)
    }
}

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
                self.on_turn(p.get("turn_id").and_then(|v| v.as_str()));
                if let Some(m) = p.get("model").and_then(|v| v.as_str()) {
                    self.c.model = m.to_string();
                }
                self.turn_skills.clear();
                None
            }
            // Tool calls inside the replayed prefix are the parent's, and are
            // counted in the parent's own file.
            "response_item" if self.replaying() => None,
            "response_item" => self.on_response_item(p, ts_ms),
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
                "token_count" => {
                    let e = self.on_token_count(p, ts_ms);
                    // A replayed snapshot still advances `prev` (inside
                    // on_token_count), so it sets this fork's baseline and the
                    // fork's own turns are measured from it — but it emits
                    // nothing, because those tokens are the parent's.
                    if self.replaying() {
                        None
                    } else {
                        e
                    }
                }
                "task_started" => {
                    self.on_turn(p.get("turn_id").and_then(|v| v.as_str()));
                    self.turn_skills.clear();
                    None
                }
                "mcp_tool_call_end" if self.replaying() => None,
                "mcp_tool_call_end" => self.on_mcp_call(p, ts_ms),
                _ => None,
            },
            _ => None,
        }
    }

    fn carry(&self) -> Option<Value> {
        serde_json::to_value(&self.c).ok()
    }

    fn quota(&self) -> Option<(i64, Value)> {
        let (ts, q) = self.c.quota.as_ref()?;
        Some((*ts, serde_json::to_value(q).ok()?))
    }
}

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
        let c = tc("2026-08-12T04:00:04.000Z", 150, 0, 0, 30);
        let ev = feed(&[META, CTX, &a, &b, &c]);
        // `b` is a decrease: it clamps to a no-op (no event), but must still
        // rebaseline `prev` to 100/10. An implementation that early-returns on
        // the decrease *without* updating `prev` would leave the pre-drop high
        // water mark (5000/200) in place, and `c` (150/30) would then also
        // clamp to zero and be silently dropped — so `ev.len()` catches it.
        assert_eq!(ev.len(), 2);
        assert!(ev.iter().all(|e| e.in_tok >= 0.0 && e.out_tok >= 0.0));
        // `c` must be measured from the rebaselined 100/10, not from 5000/200.
        assert_eq!(ev[1].in_tok, 50.0);
        assert_eq!(ev[1].out_tok, 20.0);
    }

    #[test]
    fn a_midstream_repeat_still_advances_prev_for_the_next_delta() {
        // A zero-delta snapshot (exact repeat) must also leave `prev` advanced
        // (a no-op here since cur == prev already), so a following genuine
        // delta is measured correctly rather than against a stale baseline.
        let a = tc("2026-08-12T04:00:02.000Z", 1000, 400, 0, 50);
        let b = tc("2026-08-12T04:00:03.000Z", 1500, 600, 0, 80);
        let ev = feed(&[META, CTX, &a, &a, &b]);
        assert_eq!(ev.len(), 2);
        // input +500, cached +200 → uncached 300; output +30.
        assert_eq!(ev[1].in_tok, 300.0);
        assert_eq!(ev[1].cr, 200.0);
        assert_eq!(ev[1].out_tok, 30.0);
    }

    #[test]
    fn sidechain_latches_across_a_replayed_session_meta() {
        // Real subagent files replay session_meta: the file opens with a
        // source.subagent record, then a plain "vscode" record follows a few
        // ms later (same session_id). That replay must not flip sidechain
        // back to false, or subagent tokens get misattributed to the main loop.
        let subagent_meta = r#"{"timestamp":"2026-08-12T04:00:00.000Z","type":"session_meta","payload":{"session_id":"s2","cwd":"/w/proj","source":{"subagent":{"thread_spawn":{"parent_thread_id":"p1"}}},"git":{"branch":"main"}}}"#;
        let replay_meta = r#"{"timestamp":"2026-08-12T04:00:00.003Z","type":"session_meta","payload":{"session_id":"s2","cwd":"/w/proj","source":"vscode","git":{"branch":"main"}}}"#;
        let a = tc("2026-08-12T04:00:02.000Z", 100, 0, 0, 10);
        let ev = feed(&[subagent_meta, replay_meta, CTX, &a]);
        assert_eq!(ev.len(), 1);
        assert!(ev[0].sidechain);
    }

    // ── forked rollouts replay the parent transcript ──────────────────
    //
    // Shapes taken from a real fork file
    // (rollout-…-019ff536-97a5-… , forked from …-019ff518-4ec9-…): the fork's
    // own session_meta, then the parent's session_meta and the parent's turns
    // replayed in a sub-second burst, then the fork's own first turn.
    const FORK_META: &str = r#"{"timestamp":"2026-08-12T09:03:43.706Z","type":"session_meta","payload":{"session_id":"019ff518-4ec9-7070-a0bf-955b00458f8c","id":"019ff536-97a5-7f60-8522-ce6613a468da","forked_from_id":"019ff518-4ec9-7070-a0bf-955b00458f8c","parent_thread_id":"019ff518-4ec9-7070-a0bf-955b00458f8c","cwd":"/w/proj","source":{"subagent":{"thread_spawn":{"parent_thread_id":"019ff518-4ec9-7070-a0bf-955b00458f8c","depth":1}}},"git":{"branch":"main"}}}"#;
    const PARENT_META: &str = r#"{"timestamp":"2026-08-12T09:03:43.706Z","type":"session_meta","payload":{"session_id":"019ff518-4ec9-7070-a0bf-955b00458f8c","id":"019ff518-4ec9-7070-a0bf-955b00458f8c","cwd":"/w/proj","source":"vscode","git":{"branch":"main"}}}"#;

    fn turn(ts: &str, turn_id: &str) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"task_started","turn_id":"{turn_id}"}}}}"#
        )
    }

    #[test]
    fn a_forked_rollout_counts_nothing_for_the_replayed_parent_transcript() {
        // The parent's turns (minted before the fork's own thread id) replay
        // first, carrying the parent's cumulative counters verbatim. They are
        // already counted in the parent's own file, so they must contribute
        // zero here — while still baselining the fork's counter, so the fork's
        // own turn is measured as a delta rather than as its whole cumulative.
        let p_turn1 = turn("2026-08-12T09:03:43.707Z", "019ff518-4ff6-7e72-820b-4c9df55290b9");
        let p_tc1 = tc("2026-08-12T09:03:43.718Z", 22068, 11008, 0, 288);
        let p_turn2 = turn("2026-08-12T09:03:43.723Z", "019ff535-225e-7960-b555-2be47c5403bc");
        let p_tc2 = tc("2026-08-12T09:03:43.724Z", 8196108, 4000000, 0, 90000);
        // The fork's own first turn: minted *after* 019ff536-97a5.
        let own_turn = turn("2026-08-12T09:03:43.847Z", "019ff536-98c5-76d2-a06a-5bb8f482cfe1");
        let own_tc = tc("2026-08-12T09:03:51.408Z", 8219573, 4010000, 0, 92000);

        let ev = feed(&[
            FORK_META, PARENT_META, &p_turn1, CTX, &p_tc1, &p_turn2, CTX, &p_tc2, &own_turn, CTX,
            &own_tc,
        ]);
        assert_eq!(ev.len(), 1, "only the fork's own turn may be emitted");
        // 8219573-8196108 = 23465 input, of which 4010000-4000000 = 10000 cached.
        assert_eq!(ev[0].in_tok, 13465.0);
        assert_eq!(ev[0].cr, 10000.0);
        assert_eq!(ev[0].out_tok, 2000.0);
        // The fork is still a sub-agent: the latch survives the replayed
        // session_meta, and the replay skip must not disturb it.
        assert!(ev[0].sidechain);
    }

    #[test]
    fn a_forked_rollout_drops_replayed_tool_and_mcp_records_only() {
        // mcp_tool_call_end and tool calls inside the replayed prefix are the
        // parent's (74% of this corpus's MCP calls were replays); after the
        // replay ends the fork's own must still be counted.
        let mcp = |ts: &str| {
            format!(
                r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"mcp_tool_call_end","invocation":{{"server":"github","tool":"get_pr"}}}}}}"#
            )
        };
        let exec = |ts: &str| {
            format!(
                r#"{{"timestamp":"{ts}","type":"response_item","payload":{{"type":"custom_tool_call","name":"exec","input":"cat /r/skills/review/SKILL.md"}}}}"#
            )
        };
        let p_turn = turn("2026-08-12T09:03:43.707Z", "019ff518-4ff6-7e72-820b-4c9df55290b9");
        let own_turn = turn("2026-08-12T09:03:43.847Z", "019ff536-98c5-76d2-a06a-5bb8f482cfe1");
        let ev = feed(&[
            FORK_META,
            PARENT_META,
            &p_turn,
            &mcp("2026-08-12T09:03:43.710Z"),
            &exec("2026-08-12T09:03:43.711Z"),
            &own_turn,
            &mcp("2026-08-12T09:04:10.000Z"),
            &exec("2026-08-12T09:04:11.000Z"),
        ]);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].mcp, vec!["github"]);
        assert_eq!(ev[1].skills, vec!["review"]);
    }

    #[test]
    fn an_unforked_session_is_never_treated_as_a_replay() {
        // No forked_from_id → the skip is never armed, so an ordinary session
        // (and a spawned sub-agent, which starts clean) parses exactly as
        // before, even though its first turn id predates nothing.
        let t = turn("2026-08-12T04:00:01.000Z", "019ff518-4ff6-7e72-820b-4c9df55290b9");
        let a = tc("2026-08-12T04:00:02.000Z", 1000, 400, 0, 50);
        let ev = feed(&[META, &t, CTX, &a]);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].in_tok, 600.0);
    }

    #[test]
    fn a_replayed_nested_fork_marker_cannot_re_arm_the_skip() {
        // The replayed transcript of a parent that was itself a fork carries
        // that parent's own forked_from_id. Re-arming on it would restart the
        // skip mid-file and swallow the fork's real usage, so the arm latches.
        let own_turn = turn("2026-08-12T09:03:43.847Z", "019ff536-98c5-76d2-a06a-5bb8f482cfe1");
        let nested = r#"{"timestamp":"2026-08-12T09:03:43.720Z","type":"session_meta","payload":{"session_id":"019ff518-4ec9-7070-a0bf-955b00458f8c","id":"019ff600-0000-7000-8000-000000000000","forked_from_id":"019ff400-0000-7000-8000-000000000000","cwd":"/w/proj","source":"vscode"}}"#;
        let a = tc("2026-08-12T09:04:00.000Z", 1000, 0, 0, 50);
        let b = tc("2026-08-12T09:04:01.000Z", 1600, 0, 0, 80);
        let ev = feed(&[FORK_META, PARENT_META, &own_turn, CTX, nested, &a, &b]);
        // `a` baselines nothing new (it is the first post-replay snapshot, so
        // it is a genuine delta from 0); `b` is its delta. Both counted.
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[1].in_tok, 600.0);
    }

    #[test]
    fn the_replay_skip_survives_an_incremental_read() {
        // An incremental pass can stop mid-replay. Without replay_until in
        // Carry the next pass would resume with the skip disarmed and re-count
        // the rest of the parent's transcript as fresh usage.
        let p_turn = turn("2026-08-12T09:03:43.707Z", "019ff518-4ff6-7e72-820b-4c9df55290b9");
        let p_tc = tc("2026-08-12T09:03:43.718Z", 22068, 0, 0, 288);
        let p = CodexParser;
        let mut st = p.new_file_state(None);
        for l in [FORK_META, PARENT_META, p_turn.as_str(), CTX] {
            assert!(st.parse_line(l).is_none());
        }
        let saved = st.carry().expect("codex state must be persistable");

        let mut st2 = p.new_file_state(Some(&saved));
        assert!(st2.parse_line(&p_tc).is_none(), "still inside the replay");
        let own_turn = turn("2026-08-12T09:03:43.847Z", "019ff536-98c5-76d2-a06a-5bb8f482cfe1");
        st2.parse_line(&own_turn);
        let own_tc = tc("2026-08-12T09:03:51.408Z", 32068, 0, 0, 388);
        let ev = st2.parse_line(&own_tc).expect("the fork's own turn");
        assert_eq!(ev.in_tok, 10000.0);
        assert_eq!(ev.out_tok, 100.0);
    }

    #[test]
    fn uuid7_ms_reads_the_leading_48_bits_and_rejects_non_uuids() {
        assert_eq!(
            uuid7_ms("019ff536-97a5-7f60-8522-ce6613a468da"),
            Some(0x019ff53697a5)
        );
        // Mint order is what the replay boundary relies on.
        assert!(
            uuid7_ms("019ff535-225e-7960-b555-2be47c5403bc").unwrap()
                < uuid7_ms("019ff536-97a5-7f60-8522-ce6613a468da").unwrap()
        );
        assert!(
            uuid7_ms("019ff536-98c5-76d2-a06a-5bb8f482cfe1").unwrap()
                > uuid7_ms("019ff536-97a5-7f60-8522-ce6613a468da").unwrap()
        );
        assert_eq!(uuid7_ms("not-a-uuid"), None);
        assert_eq!(uuid7_ms("short"), None);
    }

    #[test]
    fn carry_with_a_missing_field_still_restores_prev() {
        // An older/partial persisted Carry (e.g. from before a field was
        // added, or a hand-edited manifest) must still restore `prev` via
        // `#[serde(default)]`, not fail deserialization wholesale and reset
        // the cumulative baseline to zero — which would re-add an entire
        // session's tokens on the next incremental pass.
        let saved = serde_json::json!({
            "session": "s1",
            "model": "gpt-5.6-sol",
            "prev": {"input": 1000.0, "cached": 0.0, "cache_write": 0.0, "output": 50.0}
            // cwd, branch, sidechain deliberately omitted.
        });
        let p = CodexParser;
        let mut st = p.new_file_state(Some(&saved));
        let b = tc("2026-08-12T04:00:03.000Z", 1500, 0, 0, 70);
        let ev = st.parse_line(&b).expect("a delta event");
        assert_eq!(ev.in_tok, 500.0);
        assert_eq!(ev.out_tok, 20.0);
    }

    #[test]
    fn carry_resumes_the_cumulative_across_a_split_read() {
        // Every field of Carry is asserted here, because every field is set at
        // the top of the file and Carry is the only thing that carries it past
        // the first incremental pass — a pass that starts past those lines can
        // never re-derive them. `sidechain` is the sharpest case: dropping it
        // would silently re-attribute 59.7% of this machine's Codex tokens from
        // sub-agents to the main loop, a regression already found once.
        let p = CodexParser;
        // A sub-agent file, so `sidechain` is true rather than defaulted-false.
        let meta = r#"{"timestamp":"2026-08-12T04:00:00.000Z","type":"session_meta","payload":{"session_id":"s1","cwd":"/w/proj","source":{"subagent":{"other":"guardian"}},"git":{"branch":"main"}}}"#;
        let a = tc("2026-08-12T04:00:02.000Z", 1000, 0, 0, 50);
        let mut st = p.new_file_state(None);
        for l in [meta, CTX, a.as_str()] {
            st.parse_line(l);
        }
        let saved = st.carry().expect("codex state must be persistable");

        // Second pass sees only the new line but must diff against 1000/50.
        let b = tc("2026-08-12T04:00:03.000Z", 1500, 0, 0, 70);
        let mut st2 = p.new_file_state(Some(&saved));
        let ev = st2.parse_line(&b).expect("a delta event");
        // prev
        assert_eq!(ev.in_tok, 500.0);
        assert_eq!(ev.out_tok, 20.0);
        // session, model — without them the event is unattributable.
        assert_eq!(ev.session, "s1");
        assert_eq!(ev.model, "gpt-5.6-sol");
        // cwd, branch — the project and branch breakdowns.
        assert_eq!(ev.cwd, "/w/proj");
        assert_eq!(ev.branch, "main");
        // sidechain — sub-agent vs main-loop attribution.
        assert!(ev.sidechain);
        // replay_until, replay_armed — a fork file's skip must not silently
        // disarm mid-file; covered end-to-end by
        // `the_replay_skip_survives_an_incremental_read`, pinned here as fields.
        let round: Carry = serde_json::from_value(saved).expect("Carry round-trips");
        assert_eq!(round.replay_until, None, "this file declares no fork");
        assert!(!round.replay_armed);
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
    fn skill_names_labels_a_plugin_scoped_skill_as_plugin_colon_name() {
        // skills/<plugin>/<name>/SKILL.md, e.g. the real
        // skills/gstack/review/SKILL.md, is a plugin-scoped skill and must be
        // labelled the same way Claude's parser labels its own (plugin:name),
        // not dropped and not flattened to the bare name.
        assert_eq!(
            skill_names("sed -n '1,320p' /Users/u/.agents/skills/gstack/review/SKILL.md"),
            vec!["gstack:review"]
        );
        // The leading-dot exclusion still applies at the top level, even
        // though skills/.system/openai-docs/SKILL.md also matches the
        // two-level shape structurally.
        assert!(skill_names("cat /r/skills/.system/openai-docs/SKILL.md").is_empty());
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
    fn task_started_also_clears_the_turn_skill_dedup() {
        // turn_context's clear is covered above; task_started is a second,
        // separate boundary that must also clear the dedup set, or a skill
        // re-read once per turn across a long session collapses into a
        // single count for the whole file.
        let exec = |c: &str| {
            format!(
                r#"{{"timestamp":"2026-08-12T04:00:05.000Z","type":"response_item","payload":{{"type":"custom_tool_call","name":"exec","input":"{c}"}}}}"#
            )
        };
        let task_started =
            r#"{"timestamp":"2026-08-12T04:00:04.000Z","type":"event_msg","payload":{"type":"task_started"}}"#;
        let e = exec("cat /r/skills/review/SKILL.md");
        let ev = feed(&[META, CTX, &e, &e, task_started, &e]);
        let skills: Vec<&str> = ev
            .iter()
            .flat_map(|r| r.skills.iter().map(|s| s.as_str()))
            .collect();
        assert_eq!(skills, vec!["review", "review"]);
    }

    #[test]
    fn only_token_count_events_carry_a_model_so_only_they_count_as_requests() {
        // Requests are counted from events with a non-empty model. A tool, MCP
        // or skill record is not an API turn, and filling the model in on those
        // reported 8,305 requests for 4,598 real turns on this machine (+81%),
        // skewing the request trend and the "All" tab with it.
        let m = r#"{"timestamp":"2026-08-12T04:00:06.000Z","type":"event_msg","payload":{"type":"mcp_tool_call_end","invocation":{"server":"github","tool":"get_pr_info"}}}"#;
        let f = r#"{"timestamp":"2026-08-12T04:00:07.000Z","type":"response_item","payload":{"type":"function_call","name":"spawn_agent"}}"#;
        let e = r#"{"timestamp":"2026-08-12T04:00:08.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"cat /r/skills/review/SKILL.md"}}"#;
        let a = tc("2026-08-12T04:00:09.000Z", 1000, 400, 0, 50);
        let ev = feed(&[META, CTX, m, f, e, &a]);
        assert_eq!(ev.len(), 4);
        let with_model: Vec<&str> = ev
            .iter()
            .filter(|r| !r.model.is_empty())
            .map(|r| r.model.as_str())
            .collect();
        assert_eq!(with_model, vec!["gpt-5.6-sol"]);
        // The one that does carry a model is the one that carries the usage.
        assert_eq!(ev[3].in_tok, 600.0);
        // Everything else is still recorded — only the request marker is gone.
        assert_eq!(ev[0].mcp, vec!["github"]);
        assert_eq!(ev[1].tools, vec!["spawn_agent"]);
        assert_eq!(ev[2].skills, vec!["review"]);
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

}
