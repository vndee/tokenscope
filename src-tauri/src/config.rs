// Loads the "user-installed" whitelists so the dashboard only counts
// MCP servers / Skills the user actually added (PRD decision). Each account
// (config directory) has its own whitelist, so this is loaded per account:
// the config file (`.claude.json`) and the skills dir belong to one account.
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

pub struct UserConfig {
    pub mcp_servers: HashSet<String>,
    pub skills: HashSet<String>,
}

/// mcpServers (top level) + projects[*].mcpServers from a Claude `.claude.json`.
pub fn mcps_from_claude_json(config_file: &Path) -> HashSet<String> {
    let mut set = HashSet::new();
    let Some(json) = fs::read_to_string(config_file)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    else {
        return set;
    };
    if let Some(obj) = json.get("mcpServers").and_then(|v| v.as_object()) {
        for k in obj.keys() {
            set.insert(k.clone());
        }
    }
    if let Some(projects) = json.get("projects").and_then(|v| v.as_object()) {
        for proj in projects.values() {
            if let Some(obj) = proj.get("mcpServers").and_then(|v| v.as_object()) {
                for k in obj.keys() {
                    set.insert(k.clone());
                }
            }
        }
    }
    set
}

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

/// Add each subdirectory name of `dir` to the set (skills are folders), and
/// additionally register nested plugin-scoped skills as `<plugin>:<skill>`
/// when `<dir>/<plugin>/<skill>/SKILL.md` exists. The `SKILL.md` check is
/// what tells a real nested skill apart from a skill's own support
/// directories (`references/`, `scripts/`, `assets/`), which must not become
/// whitelist entries. Directory names beginning with `.` are skipped at both
/// levels.
///
/// A skill reachable two ways from the same root counts once. Roots commonly
/// symlink plugin skills in flat (`~/.claude/skills/review -> gstack/review`),
/// and registering the nested form on top of the flat one counted 31 of 80
/// installed skills twice on this machine. Only nested skills with no bare
/// entry of their own are genuinely newly-reachable, and only those are added.
/// The check is per root, because reachability is: a flat symlink in one root
/// says nothing about a plugin skill in another.
///
/// Two passes rather than one, so the flat names are all known before any
/// nested name is judged against them — otherwise readdir order would decide
/// whether a duplicate was caught.
fn scan_skill_dir(dir: &Path, set: &mut HashSet<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut top: Vec<(String, PathBuf)> = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = e.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        top.push((name, path));
    }
    let flat: HashSet<&str> = top.iter().map(|(n, _)| n.as_str()).collect();

    for (name, path) in &top {
        set.insert(name.clone());

        let Ok(nested_entries) = fs::read_dir(path) else {
            continue;
        };
        for ne in nested_entries.flatten() {
            let nested_path = ne.path();
            if !nested_path.is_dir() {
                continue;
            }
            let Some(nested_name) = ne.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if nested_name.starts_with('.') || flat.contains(nested_name.as_str()) {
                continue;
            }
            if nested_path.join("SKILL.md").is_file() {
                set.insert(format!("{name}:{nested_name}"));
            }
        }
    }
}

impl UserConfig {
    /// Load one account's whitelist from its `.claude.json` and skills dir.
    /// `config_file` is the account's .claude.json; `skills_dir` is its
    /// `<config-dir>/skills/`. Project-level skill dirs are intentionally not
    /// scanned (PRD §3.3: the skill source is the account's global skills dir).
    pub fn load_for(config_file: &Path, skills_dir: &Path) -> Self {
        UserConfig {
            mcp_servers: mcps_from_claude_json(config_file),
            skills: skills_from_dirs(std::slice::from_ref(&skills_dir.to_path_buf())),
        }
    }

    /// A tool name like "mcp__<server>__<tool>" → is server user-installed?
    pub fn is_user_mcp(&self, server: &str) -> bool {
        self.mcp_servers.contains(server)
    }

    /// A skill id (may be "plugin:skill") → user-installed?
    ///
    /// Tries the full key first, so a nested/plugin-scoped skill registered
    /// as `<plugin>:<skill>` (see `scan_skill_dir`) matches on its own name
    /// rather than colliding with an unrelated top-level skill that happens
    /// to share the suffix (e.g. `gstack:review` vs. top-level `review`).
    /// Falls back to the stripped suffix, kept deliberately: it is what lets
    /// Claude plugin skills that live outside any skills root (e.g. under
    /// `~/.claude/plugins/cache/**/skills/`) still count.
    pub fn is_user_skill(&self, skill: &str) -> bool {
        if self.skills.contains(skill) {
            return true;
        }
        let key = skill.rsplit(':').next().unwrap_or(skill);
        self.skills.contains(key)
    }
}

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

    #[test]
    fn nested_skill_with_skill_md_is_registered_as_plugin_colon_skill() {
        let root = std::env::temp_dir().join(format!("ts-skills-nested-{}", std::process::id()));
        let skills = root.join("skills");
        let plugin_skill = skills.join("gstack").join("review");
        let _ = fs::create_dir_all(&plugin_skill);
        fs::write(plugin_skill.join("SKILL.md"), "# review").unwrap();

        let got = skills_from_dirs(&[skills]);
        assert!(got.contains("gstack:review"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn nested_dir_without_skill_md_is_not_registered() {
        // A skill's own support directory (e.g. `references/`) must not
        // become a whitelist entry — only a real nested skill, distinguished
        // by having its own SKILL.md, may.
        let root =
            std::env::temp_dir().join(format!("ts-skills-no-skillmd-{}", std::process::id()));
        let skills = root.join("skills");
        let skill_dir = skills.join("review");
        let refs_dir = skill_dir.join("references");
        let _ = fs::create_dir_all(&refs_dir);
        fs::write(skill_dir.join("SKILL.md"), "# review").unwrap();

        let got = skills_from_dirs(&[skills]);
        assert!(got.contains("review"));
        assert!(!got.contains("review:references"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn nested_registration_is_additive_to_top_level_names() {
        // Every name a plain one-level scan would register must still be
        // registered once nested plugin-scoped skills are added too.
        let root = std::env::temp_dir().join(format!("ts-skills-additive-{}", std::process::id()));
        let skills = root.join("skills");
        let _ = fs::create_dir_all(skills.join("review"));
        let _ = fs::create_dir_all(skills.join("payment-integration"));
        let plugin_skill = skills.join("gstack").join("plan-eng-review");
        let _ = fs::create_dir_all(&plugin_skill);
        fs::write(plugin_skill.join("SKILL.md"), "# plan-eng-review").unwrap();

        let got = skills_from_dirs(&[skills]);

        // Names the one-level scan would have registered are all still here.
        assert!(got.contains("review"));
        assert!(got.contains("payment-integration"));
        assert!(got.contains("gstack"));
        // Plus the new nested entry, purely additive.
        assert!(got.contains("gstack:plan-eng-review"));
        assert_eq!(got.len(), 4);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_skill_reachable_flat_and_nested_is_registered_once() {
        // The real ~/.claude/skills symlinks every plugin skill in flat
        // (review -> gstack/review), so registering the nested form too counted
        // 31 of 80 skills a second time — 80 installed skills reported as 111,
        // with not one of them newly reachable.
        let root = std::env::temp_dir().join(format!("ts-skills-dedup-{}", std::process::id()));
        let skills = root.join("skills");
        let plugin = skills.join("gstack");
        for n in ["review", "ship"] {
            let d = plugin.join(n);
            let _ = fs::create_dir_all(&d);
            fs::write(d.join("SKILL.md"), "# s").unwrap();
        }
        // `review` is also reachable flat; `ship` is not.
        let flat = skills.join("review");
        let _ = fs::create_dir_all(&flat);
        fs::write(flat.join("SKILL.md"), "# review").unwrap();

        let got = skills_from_dirs(&[skills]);
        assert!(got.contains("review"));
        assert!(!got.contains("gstack:review"), "already reachable as `review`");
        // The nested skill with no flat entry is what the nested pass is for.
        assert!(got.contains("gstack:ship"));
        assert!(got.contains("gstack"));
        assert_eq!(got.len(), 3);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_nested_skill_shadowed_in_another_root_is_still_registered() {
        // Reachability is per root: a flat `review` in one skills dir does not
        // make `gstack/review` in a *different* dir reachable by its bare name,
        // so suppressing it there would drop a real entry.
        let root = std::env::temp_dir().join(format!("ts-skills-xroot-{}", std::process::id()));
        let a = root.join("a/skills");
        let b = root.join("b/skills");
        let _ = fs::create_dir_all(a.join("review"));
        let nested = b.join("gstack").join("review");
        let _ = fs::create_dir_all(&nested);
        fs::write(nested.join("SKILL.md"), "# review").unwrap();

        let got = skills_from_dirs(&[a, b]);
        assert!(got.contains("review"));
        assert!(got.contains("gstack:review"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn is_user_skill_matches_full_key_before_falling_back_to_stripped_suffix() {
        let mut skills = HashSet::new();
        skills.insert("review".to_string());
        skills.insert("gstack".to_string());
        skills.insert("gstack:review".to_string());
        let cfg = UserConfig {
            mcp_servers: HashSet::new(),
            skills: skills.clone(),
        };
        // Full key matches its own registered entry, not the unrelated
        // top-level "review".
        assert!(cfg.is_user_skill("gstack:review"));

        // Still true even when the unrelated top-level "review" is absent —
        // proving the match is on the full key, not the stripped collision.
        skills.remove("review");
        let cfg2 = UserConfig {
            mcp_servers: HashSet::new(),
            skills,
        };
        assert!(cfg2.is_user_skill("gstack:review"));
    }

    #[test]
    fn dot_prefixed_directories_are_skipped_at_both_levels() {
        let root = std::env::temp_dir().join(format!("ts-skills-dotskip-{}", std::process::id()));
        let skills = root.join("skills");
        let _ = fs::create_dir_all(skills.join(".hidden-top"));
        let dotted_nested = skills.join("gstack").join(".hidden-nested");
        let _ = fs::create_dir_all(&dotted_nested);
        fs::write(dotted_nested.join("SKILL.md"), "# hidden").unwrap();

        let got = skills_from_dirs(&[skills]);
        assert!(!got.contains(".hidden-top"));
        assert!(!got.contains("gstack:.hidden-nested"));
        // The non-dotted plugin dir itself is still registered.
        assert!(got.contains("gstack"));

        let _ = fs::remove_dir_all(&root);
    }
}
