// Discovery of Claude CLI "accounts" — one per config directory.
//
// Claude Code's default account keeps its config at ~/.claude.json and its data
// (projects/, skills/) under ~/.claude/. A second account created with
// `CLAUDE_CONFIG_DIR=~/.claude-work` keeps BOTH under that dir
// (~/.claude-work/.claude.json + ~/.claude-work/projects/). We model an account
// as a (data_dir, config_file) pair and auto-discover them so the dashboard can
// show one tab per account plus an aggregate — without relying on an env var at
// launch (a login LaunchAgent wouldn't inherit CLAUDE_CONFIG_DIR anyway).
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub struct Account {
    pub id: String,           // slug of data_dir; namespaces the per-account cache + tab key
    pub label: String,        // friendly name (org / display name / email / dir)
    pub email: String,        // account email if known (may be empty)
    pub data_dir: PathBuf,    // holds projects/ and skills/
    pub config_file: PathBuf, // the .claude.json for this account
}

/// A filesystem-safe, stable id derived from the config dir path (so two
/// accounts never share an events cache, and the tab key survives restarts).
fn slug(p: &Path) -> String {
    p.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Friendly label + email from an account's .claude.json `oauthAccount` block.
/// Prefers a real organization name, then the display name, then the email
/// local-part, then the directory name. The auto-generated personal org
/// ("<email>'s Organization") is treated as no real org, so a personal account
/// shows its display name instead of that noise.
fn label_and_email(config_file: &Path, data_dir: &Path) -> (String, String) {
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

fn try_add(out: &mut Vec<Account>, seen: &mut HashSet<PathBuf>, data_dir: PathBuf, config_file: PathBuf) {
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
    out.push(Account {
        id: slug(&data_dir),
        label,
        email,
        data_dir,
        config_file,
    });
}

/// All Claude accounts on this machine, in a stable order (default first).
pub fn discover() -> Vec<Account> {
    let mut out: Vec<Account> = Vec::new();
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
