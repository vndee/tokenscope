// Agent adapters. Everything that differs between the CLIs Tokenscope tracks —
// where the logs live, how a log line becomes a RawEvent, which whitelist file
// to read — lives behind these traits, so store.rs stays generic and a new CLI
// is a new file here rather than a new branch everywhere.
pub mod claude;
pub mod codex;

use crate::config::UserConfig;
use crate::store::RawEvent;
use std::path::{Path, PathBuf};

/// One tracked account: a single agent CLI's config directory.
pub struct AccountSpec {
    /// Slug of the config dir. Namespaces the incremental cache and is the tab key.
    pub id: String,
    /// Owning agent's descriptor id ("claude" / "codex").
    pub agent: &'static str,
    pub label: String,
    pub email: String,
    /// Directory walked for *.jsonl logs.
    pub log_root: PathBuf,
    pub config_file: PathBuf,
    pub skill_dirs: Vec<PathBuf>,
}

/// Per-file parsing state. Created fresh for each source file so streaming
/// state (Codex's cumulative token counter) resets exactly when a file is
/// re-read from byte 0.
pub trait FileState {
    fn parse_line(&mut self, line: &str) -> Option<RawEvent>;
    /// State to persist in the manifest so the next incremental pass over this
    /// same file resumes exactly where this one stopped. `None` = stateless.
    fn carry(&self) -> Option<serde_json::Value> {
        None
    }
}

pub trait LogParser: Send + Sync {
    fn new_file_state(&self, carry: Option<&serde_json::Value>) -> Box<dyn FileState>;
}

pub struct AgentDescriptor {
    pub id: &'static str,
    pub display: &'static str,
    pub discover: fn() -> Vec<AccountSpec>,
    pub load_config: fn(&AccountSpec) -> UserConfig,
    pub parser: fn() -> Box<dyn LogParser>,
}

static REGISTRY: &[AgentDescriptor] = &[claude::DESCRIPTOR];

pub fn registry() -> &'static [AgentDescriptor] {
    REGISTRY
}

/// Every account across every agent, in registry order (Claude first).
pub fn discover_all() -> Vec<(&'static AgentDescriptor, AccountSpec)> {
    let mut out = Vec::new();
    for d in registry() {
        for a in (d.discover)() {
            out.push((d, a));
        }
    }
    out
}

/// A filesystem-safe, stable id derived from a config dir path, so two accounts
/// never share an events cache and a tab key survives restarts.
pub fn slug(p: &Path) -> String {
    p.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}
