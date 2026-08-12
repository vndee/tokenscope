// Codex CLI adapter. Logs live at <codex-dir>/sessions/YYYY/MM/DD/rollout-*.jsonl,
// one session per file. See docs/superpowers/specs/2026-08-12-codex-tracking-design.md.
use super::{AccountSpec, FileState, LogParser};
use crate::config::{self, UserConfig};
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
