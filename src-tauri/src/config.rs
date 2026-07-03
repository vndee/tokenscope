// Loads the "user-installed" whitelists so the dashboard only counts
// MCP servers / Skills the user actually added (PRD decision). Each account
// (config directory) has its own whitelist, so this is loaded per account:
// the config file (`.claude.json`) and the skills dir belong to one account.
use std::collections::HashSet;
use std::fs;
use std::path::Path;

pub struct UserConfig {
    pub mcp_servers: HashSet<String>,
    pub skills: HashSet<String>,
}

/// mcpServers (top level) + projects[*].mcpServers from a parsed .claude.json.
fn mcps_from(json: Option<&serde_json::Value>) -> HashSet<String> {
    let mut set = HashSet::new();
    let Some(json) = json else { return set };
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
        let json = fs::read_to_string(config_file)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
        let mut skills = HashSet::new();
        scan_skill_dir(skills_dir, &mut skills);
        UserConfig {
            mcp_servers: mcps_from(json.as_ref()),
            skills,
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
