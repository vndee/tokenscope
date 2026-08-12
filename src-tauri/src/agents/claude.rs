// Claude Code agent adapter: discovery of Claude CLI "accounts" (one per
// config directory) and parsing of its JSONL session logs.
//
// Claude Code's default account keeps its config at ~/.claude.json and its data
// (projects/, skills/) under ~/.claude/. A second account created with
// `CLAUDE_CONFIG_DIR=~/.claude-work` keeps BOTH under that dir
// (~/.claude-work/.claude.json + ~/.claude-work/projects/). We model an account
// as a (data_dir, config_file) pair and auto-discover them so the dashboard can
// show one tab per account plus an aggregate — without relying on an env var at
// launch (a login LaunchAgent wouldn't inherit CLAUDE_CONFIG_DIR anyway).
use super::{AccountSpec, AgentDescriptor, FileState, LogParser};
use crate::config::UserConfig;
use crate::store::RawEvent;
use chrono::DateTime;
use std::collections::HashSet;
use std::path::PathBuf;

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

/// Friendly label + email from an account's .claude.json `oauthAccount` block.
/// Prefers a real organization name, then the display name, then the email
/// local-part, then the directory name. The auto-generated personal org
/// ("<email>'s Organization") is treated as no real org, so a personal account
/// shows its display name instead of that noise.
fn label_and_email(config_file: &std::path::Path, data_dir: &std::path::Path) -> (String, String) {
    let dir_name = data_dir
        .file_name()
        .map(|s| s.to_string_lossy().trim_start_matches('.').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "account".into());

    let oa = std::fs::read_to_string(config_file)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|j| j.get("oauthAccount").cloned());
    let get = |k: &str| -> Option<String> {
        oa.as_ref()
            .and_then(|o| o.get(k))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
    };

    let email = get("emailAddress").unwrap_or_default();
    let org = get("organizationName")
        .filter(|o| !o.contains('@') && !o.ends_with("'s Organization"));
    let label = org
        .or_else(|| get("displayName"))
        .or_else(|| {
            email
                .split('@')
                .next()
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or(dir_name);
    (label, email)
}

fn try_add(out: &mut Vec<AccountSpec>, seen: &mut HashSet<PathBuf>, data_dir: PathBuf, config_file: PathBuf) {
    // Only a directory that actually has a projects/ log dir is an account.
    if !data_dir.join("projects").is_dir() {
        return;
    }
    // Dedupe by canonical path so ~/.claude, $CLAUDE_CONFIG_DIR and the sibling
    // scan can't register the same account twice.
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

/// All Claude accounts on this machine, in a stable order (default first).
fn discover() -> Vec<AccountSpec> {
    let mut out: Vec<AccountSpec> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let Some(home) = dirs::home_dir() else {
        return out;
    };

    // 1. Default account: config at ~/.claude.json, data under ~/.claude/.
    try_add(&mut out, &mut seen, home.join(".claude"), home.join(".claude.json"));

    // 2. CLAUDE_CONFIG_DIR, if the process happens to have it (config + data
    //    both live under that dir). Usually absent for a login-launched app.
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        let d = PathBuf::from(dir);
        let cfg = d.join(".claude.json");
        try_add(&mut out, &mut seen, d, cfg);
    }

    // 3. Sibling ~/.claude* config dirs (e.g. ~/.claude-work) — the robust path
    //    that works even at login without any env var. Sorted for stable tabs.
    if let Ok(entries) = std::fs::read_dir(&home) {
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with(".claude") && n != ".claude")
                        .unwrap_or(false)
            })
            .collect();
        dirs.sort();
        for d in dirs {
            let cfg = d.join(".claude.json");
            try_add(&mut out, &mut seen, d, cfg);
        }
    }

    out
}

/// Parse one JSONL line into a RawEvent (assistant messages only).
fn parse_line(line: &str) -> Option<RawEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    match v.get("type")?.as_str()? {
        "assistant" => parse_assistant(&v),
        "user" => parse_user(&v),
        _ => None,
    }
}

/// A user message is either a slash-command invocation (string content) or a
/// batch of tool_result blocks (array content). Route to the right extractor.
fn parse_user(v: &serde_json::Value) -> Option<RawEvent> {
    let content = v.get("message")?.get("content")?;
    if let Some(text) = content.as_str() {
        return parse_user_command(v, text);
    }
    let arr = content.as_array()?;
    let mut results = 0u32;
    let mut errors = 0u32;
    for b in arr {
        if b.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
            results += 1;
            if b.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false) {
                errors += 1;
            }
        }
    }
    if results == 0 {
        return None;
    }
    let ts = v.get("timestamp")?.as_str()?;
    let ts_ms = DateTime::parse_from_rfc3339(ts).ok()?.timestamp_millis();
    // dedup key: the line's own uuid (tool_result messages carry no message.id)
    let id = v.get("uuid").and_then(|i| i.as_str()).unwrap_or("").to_string();
    if id.is_empty() {
        return None;
    }
    Some(RawEvent {
        ts_ms,
        session: v.get("sessionId").and_then(|s| s.as_str()).unwrap_or("").to_string(),
        model: String::new(), // not an LLM request → no model/tokens
        in_tok: 0.0,
        cc: 0.0,
        cr: 0.0,
        out_tok: 0.0,
        mcp: Vec::new(),
        skills: Vec::new(),
        id,
        source: String::new(),
        cwd: String::new(),
        branch: String::new(),
        tools: Vec::new(),
        sidechain: false,
        tool_results: results,
        tool_errors: errors,
    })
}

/// Extract the inner text of `<tag>...</tag>` from `s`, if present.
fn extract_tag(s: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = s.find(&open)? + open.len();
    let rest = &s[start..];
    let end = rest.find(&close)?;
    Some(rest[..end].to_string())
}

/// A user message that is a slash-command invocation of a skill, e.g.
/// `<command-name>/find-skills</command-name>`. The skill name is left
/// unfiltered here; compute_event drops non-user skills via the whitelist.
fn parse_user_command(v: &serde_json::Value, text: &str) -> Option<RawEvent> {
    let raw = extract_tag(text, "command-name")?;
    let skill = raw.trim().trim_start_matches('/').trim().to_string();
    if skill.is_empty() {
        return None;
    }
    let ts = v.get("timestamp")?.as_str()?;
    let ts_ms = DateTime::parse_from_rfc3339(ts).ok()?.timestamp_millis();
    let session = v
        .get("sessionId")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    // dedup key: the line's own uuid (command messages have no message.id)
    let id = v.get("uuid").and_then(|i| i.as_str())?.to_string();
    if id.is_empty() {
        return None;
    }
    Some(RawEvent {
        ts_ms,
        session,
        model: String::new(), // not an LLM request → no model/tokens/cost
        in_tok: 0.0,
        cc: 0.0,
        cr: 0.0,
        out_tok: 0.0,
        mcp: Vec::new(),
        skills: vec![skill],
        id,
        source: String::new(),
        cwd: v.get("cwd").and_then(|c| c.as_str()).unwrap_or("").to_string(),
        branch: v.get("gitBranch").and_then(|b| b.as_str()).unwrap_or("").to_string(),
        tools: Vec::new(),
        sidechain: false,
        tool_results: 0,
        tool_errors: 0,
    })
}

fn parse_assistant(v: &serde_json::Value) -> Option<RawEvent> {
    let msg = v.get("message")?;
    let model = msg.get("model").and_then(|m| m.as_str()).unwrap_or("unknown");
    if model == "<synthetic>" {
        return None;
    }
    let ts = v.get("timestamp")?.as_str()?;
    let ts_ms = DateTime::parse_from_rfc3339(ts).ok()?.timestamp_millis();
    let session = v
        .get("sessionId")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let id = msg
        .get("id")
        .and_then(|i| i.as_str())
        .unwrap_or("")
        .to_string();

    let usage = msg.get("usage");
    let g = |k: &str| -> f64 {
        usage
            .and_then(|u| u.get(k))
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0)
    };

    let mut mcp = Vec::new();
    let mut skills = Vec::new();
    let mut tools = Vec::new();
    if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
        for block in content {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if !name.is_empty() {
                tools.push(name.to_string());
            }
            if let Some(rest) = name.strip_prefix("mcp__") {
                mcp.push(rest.split("__").next().unwrap_or("").to_string());
            } else if name == "Skill" {
                if let Some(sk) = block
                    .get("input")
                    .and_then(|i| i.get("skill"))
                    .and_then(|s| s.as_str())
                {
                    if !sk.is_empty() {
                        skills.push(sk.to_string());
                    }
                }
            }
        }
    }

    Some(RawEvent {
        ts_ms,
        session,
        model: model.to_string(),
        in_tok: g("input_tokens"),
        cc: g("cache_creation_input_tokens"),
        cr: g("cache_read_input_tokens"),
        out_tok: g("output_tokens"),
        mcp,
        skills,
        id,
        source: String::new(),
        cwd: v.get("cwd").and_then(|c| c.as_str()).unwrap_or("").to_string(),
        branch: v.get("gitBranch").and_then(|b| b.as_str()).unwrap_or("").to_string(),
        tools,
        sidechain: v.get("isSidechain").and_then(|b| b.as_bool()).unwrap_or(false),
        tool_results: 0,
        tool_errors: 0,
    })
}
