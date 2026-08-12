// Codex CLI adapter. Logs live at <codex-dir>/sessions/YYYY/MM/DD/rollout-*.jsonl,
// one session per file. See docs/superpowers/specs/2026-08-12-codex-tracking-design.md.
use super::{AccountSpec, FileState, LogParser};
use crate::config::{self, UserConfig};
use crate::store::RawEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;

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
                if let Some(m) = p.get("model").and_then(|v| v.as_str()) {
                    self.c.model = m.to_string();
                }
                self.turn_skills.clear();
                None
            }
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
                "token_count" => self.on_token_count(p, ts_ms),
                "task_started" => {
                    self.turn_skills.clear();
                    None
                }
                "mcp_tool_call_end" => self.on_mcp_call(p, ts_ms),
                _ => None,
            },
            _ => None,
        }
    }

    fn carry(&self) -> Option<Value> {
        serde_json::to_value(&self.c).ok()
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
}
