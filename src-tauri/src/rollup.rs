// Durable per-day rollup archive.
//
// The raw event store is pruned to 210 days (see parser.rs), and pruned events
// never come back — an old log already read to EOF is never re-read. This file
// is what makes "all time" mean all time: one small, self-describing row per
// calendar day, in its own never-pruned document.
//
// Two things are deliberately NOT baked into a row, because the app applies
// them at read time and they must stay retroactive: MCP/skill names are stored
// unfiltered (the whitelist is applied when the row is read), and per-model
// tokens are stored raw (prices are applied when the row is read).
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;

// Bump when a row's meaning changes. A mismatch discards the archive whole:
// up to 210 days rebuild from the raw store, and older history is lost, which
// is strictly better than silently misreading it.
pub const ROLLUP_VERSION: u32 = 1;

/// Raw token components for one model on one day, plus its request count.
/// Kept raw so cost is re-derived from the *current* price table on read.
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct TokBits {
    pub input: f64,
    pub cc: f64,
    pub cr: f64,
    pub out: f64,
    pub requests: u64,
}

/// One calendar day of usage for one account.
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct DayRow {
    pub date: String, // ISO yyyy-mm-dd
    /// RAW model id (the price-lookup key), not the normalized display name.
    #[serde(default)]
    pub models: HashMap<String, TokBits>,
    /// (M tokens, USD). These costs are frozen at archive time — deriving them
    /// on read would need a project×model cross product per day.
    #[serde(default)]
    pub projects: HashMap<String, (f64, f64)>,
    #[serde(default)]
    pub branches: HashMap<String, (f64, f64)>,
    #[serde(default)]
    pub accounts: HashMap<String, (f64, f64)>,
    /// Unfiltered names — the whitelist is applied on read.
    #[serde(default)]
    pub tools: HashMap<String, u64>,
    #[serde(default)]
    pub mcp: HashMap<String, u64>,
    #[serde(default)]
    pub skills: HashMap<String, u64>,
    /// Hour-of-day token histogram, M tokens.
    #[serde(default)]
    pub hourly: Vec<f64>,
    /// Sessions distinct *within this day*. Summing across days double-counts a
    /// session that spans local midnight; the alternative is archiving an
    /// unbounded id set. Bounded at one per crossing, and accepted.
    #[serde(default)]
    pub sessions: u64,
    /// Raw tokens spent inside subagents (isSidechain).
    #[serde(default)]
    pub subagent: f64,
    #[serde(default)]
    pub tool_results: u64,
    #[serde(default)]
    pub tool_errors: u64,
}

impl DayRow {
    pub fn new(date: &str) -> Self {
        DayRow {
            date: date.to_string(),
            hourly: vec![0.0; 24],
            ..Default::default()
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
pub struct Archive {
    /// ISO date -> row. A date-keyed map is what makes the archive/live split
    /// structural: absorbing a live day overwrites its row instead of adding a
    /// second copy, so no day can ever be counted twice.
    pub days: BTreeMap<String, DayRow>,
}

#[derive(Serialize)]
struct DocRef<'a> {
    version: u32,
    days: &'a BTreeMap<String, DayRow>,
}

#[derive(Deserialize)]
struct Doc {
    version: u32,
    days: BTreeMap<String, DayRow>,
}

fn write_atomic(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)
}

fn cache_dir() -> Option<PathBuf> {
    let d = dirs::cache_dir()?.join("tokenscope");
    let _ = fs::create_dir_all(&d);
    Some(d)
}

impl Archive {
    pub fn load(id: &str) -> Self {
        match cache_dir() {
            Some(d) => Self::load_from(&d, id),
            None => Archive::default(),
        }
    }

    /// `load`, against an explicit cache directory (so it is testable).
    fn load_from(dir: &std::path::Path, id: &str) -> Self {
        fs::read_to_string(dir.join(format!("rollup-{id}.json")))
            .ok()
            .and_then(|t| serde_json::from_str::<Doc>(&t).ok())
            .filter(|d| d.version == ROLLUP_VERSION)
            .map(|d| Archive { days: d.days })
            .unwrap_or_default()
    }

    pub fn save(&self, id: &str) {
        if let Some(d) = cache_dir() {
            self.save_to(&d, id);
        }
    }

    /// `save`, against an explicit cache directory (so it is testable).
    fn save_to(&self, dir: &std::path::Path, id: &str) {
        let doc = DocRef {
            version: ROLLUP_VERSION,
            days: &self.days,
        };
        if let Ok(t) = serde_json::to_string(&doc) {
            let _ = write_atomic(&dir.join(format!("rollup-{id}.json")), t.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(date: &str, out: f64) -> DayRow {
        let mut r = DayRow::new(date);
        r.models.insert(
            "claude-opus-5".to_string(),
            TokBits { input: 10.0, cc: 0.0, cr: 0.0, out, requests: 1 },
        );
        r
    }

    #[test]
    fn an_archive_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("ts-roll-rt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);

        let mut a = Archive::default();
        a.days.insert("2026-01-05".to_string(), row("2026-01-05", 7.0));
        a.save_to(&dir, "acct");

        let back = Archive::load_from(&dir, "acct");
        assert_eq!(back.days.len(), 1);
        assert_eq!(back.days["2026-01-05"].models["claude-opus-5"].out, 7.0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_archive_from_an_older_version_is_discarded_whole() {
        // A format change must lose history rather than misread it: up to 210
        // days rebuild themselves from the raw store on the next build.
        let dir = std::env::temp_dir().join(format!("ts-roll-ver-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        fs::write(
            dir.join("rollup-acct.json"),
            serde_json::json!({
                "version": ROLLUP_VERSION - 1,
                "days": { "2026-01-05": { "date": "2026-01-05" } }
            })
            .to_string(),
        )
        .unwrap();

        assert!(Archive::load_from(&dir, "acct").days.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_archive_is_discarded_whole() {
        let dir = std::env::temp_dir().join(format!("ts-roll-trunc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);

        let mut a = Archive::default();
        a.days.insert("2026-01-05".to_string(), row("2026-01-05", 7.0));
        a.save_to(&dir, "acct");

        let p = dir.join("rollup-acct.json");
        let whole = fs::read_to_string(&p).unwrap();
        fs::write(&p, &whole[..whole.len() / 2]).unwrap();
        assert!(Archive::load_from(&dir, "acct").days.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }
}
