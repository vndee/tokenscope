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

/// Add each subdirectory name of `dir` to the set (skills are folders).
fn scan_skill_dir(dir: &Path, set: &mut HashSet<String>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.path().is_dir() {
                if let Some(name) = e.file_name().to_str() {
                    set.insert(name.to_string());
                }
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

    /// A skill id (may be "plugin:skill") → strip plugin prefix, check dir.
    pub fn is_user_skill(&self, skill: &str) -> bool {
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
}
