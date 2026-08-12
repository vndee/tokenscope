// Incremental event store.
//
// Ingestion (this file) is the only place that touches the JSONL logs. It
// parses each assistant message into a provider/config/price-independent
// RawEvent (just the facts), reads only newly-appended bytes of changed files
// (tracked by a per-file size/mtime/offset manifest), dedupes by message id,
// and persists everything to the cache dir. Aggregation (parser.rs) then works
// purely on these in-memory events — cheap, and recomputed per request because
// the Day/Week/Month windows are relative to "now".
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use walkdir::WalkDir;

#[derive(Serialize, Deserialize, Clone)]
pub struct RawEvent {
    pub ts_ms: i64,
    pub session: String,
    pub model: String, // raw model id (price lookup), normalized later for grouping
    pub in_tok: f64,
    pub cc: f64, // cache creation
    pub cr: f64, // cache read
    pub out_tok: f64,
    pub mcp: Vec<String>,    // all mcp__<server> names called (unfiltered)
    pub skills: Vec<String>, // all Skill input.skill ids called (unfiltered)
    pub id: String,          // message id (dedup)
    // Source log file (manifest key). Lets a truncated/rewritten file purge its
    // own stale events before being re-read, so re-ingestion stays idempotent.
    #[serde(default)]
    pub source: String,
    // Working directory of the session (its basename is the "project").
    #[serde(default)]
    pub cwd: String,
    // git branch checked out during the session (may be empty / detached HEAD).
    #[serde(default)]
    pub branch: String,
    // Every tool_use name in this message (built-in + mcp__ + Skill) — powers the
    // full tool-usage breakdown, distinct from the mcp/skill whitelisted views.
    #[serde(default)]
    pub tools: Vec<String>,
    // isSidechain: this assistant turn ran inside a subagent, not the main loop.
    #[serde(default)]
    pub sidechain: bool,
    // Reliability: tool_result blocks in a user message and how many were errors
    // (`is_error`). Zero for assistant / slash-command events.
    #[serde(default)]
    pub tool_results: u32,
    #[serde(default)]
    pub tool_errors: u32,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct FileEntry {
    size: u64,
    mtime_ms: i64,
    /// Bytes of this file already ingested.
    offset: u64,
    /// Parser state at that offset, so an incremental read resumes exactly
    /// (Codex diffs a cumulative token counter and cannot restart from zero).
    #[serde(default)]
    carry: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Default)]
struct Manifest {
    files: HashMap<String, FileEntry>,
}

/// One account's whole cache. The events, the byte-offset manifest and the
/// store version are a single consistency unit — an offset only means anything
/// relative to the events that were loaded — so they live in one document,
/// written under one atomic rename. Split across separate files they could skew
/// against each other if a crash landed between two writes; see `save`.
#[derive(Deserialize)]
struct Snapshot {
    version: u32,
    events: Vec<RawEvent>,
    manifest: Manifest,
}

/// Borrowing twin of `Snapshot`, so saving doesn't clone the event vector.
#[derive(Serialize)]
struct SnapshotRef<'a> {
    version: u32,
    events: &'a [RawEvent],
    manifest: &'a Manifest,
}

pub struct Store {
    pub events: Vec<RawEvent>,
    // message id -> index in `events`. A single assistant message can be split
    // across several JSONL lines (e.g. thinking on one line, tool_use on the
    // next) that all share its id; we merge their tool calls into one event and
    // count its token usage only once.
    index: HashMap<String, usize>,
    manifest: Manifest,
}

// Bump when the parsing/extraction logic changes in a way that requires
// re-reading logs from scratch (the incremental manifest would otherwise skip
// already-seen bytes and miss newly-extracted facts).
//   v2: count slash-command skill invocations (`/skill`), not just Skill tool_use.
//   v3: merge tool_use across lines sharing a message id (a thinking line + a
//       tool_use line were deduped, dropping the tool call).
//   v4: track a per-event source file (idempotent re-read of truncated logs).
//   v5: capture cwd (project), git branch, full tool list, and subagent flag.
//   v6: count tool_result blocks + errors (is_error) from user messages.
//   v7: per-file parser carry in the manifest (Codex cumulative token deltas).
//   v8: skip the parent transcript a forked Codex rollout replays (it was
//       counted as fresh usage, at the fork's timestamp).
const STORE_VERSION: u32 = 8;

/// Atomically replace `path`'s contents: write a sibling temp file, then rename
/// over the target (same-volume rename is atomic on Windows and Unix). Avoids
/// the half-written/truncated JSON that a crash mid-`fs::write` would leave.
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

impl Store {
    /// Load persisted events + offset manifest for one account (empty on first
    /// run). Cache files are namespaced by the account `id` so multiple accounts
    /// never share (or clobber) each other's incremental state.
    pub fn load(id: &str) -> Self {
        let Some(dir) = cache_dir() else {
            return Store {
                events: Vec::new(),
                index: HashMap::new(),
                manifest: Manifest::default(),
            };
        };
        Self::load_from(&dir, id)
    }

    /// `load`, against an explicit cache directory (so it is testable).
    fn load_from(dir: &std::path::Path, id: &str) -> Self {
        let mut events: Vec<RawEvent> = Vec::new();
        let mut manifest = Manifest::default();
        // A snapshot that is missing, truncated by a crash mid-write, or written
        // by an older parser is discarded whole, and ingest() does a full
        // rescan. Nothing partial is ever adopted: a manifest without its events
        // would make ingest() skip every already-recorded file and silently lose
        // all history.
        if let Some(s) = fs::read_to_string(dir.join(format!("store-{id}.json")))
            .ok()
            .and_then(|t| serde_json::from_str::<Snapshot>(&t).ok())
        {
            if s.version == STORE_VERSION {
                events = s.events;
                manifest = s.manifest;
            }
        }
        let index = events
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.id.is_empty())
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        Store {
            events,
            index,
            manifest,
        }
    }

    pub fn save(&self, id: &str) {
        if let Some(dir) = cache_dir() {
            self.save_to(&dir, id);
        }
    }

    /// `save`, against an explicit cache directory (so it is testable).
    ///
    /// The events and the offset manifest go out as ONE document under ONE
    /// atomic rename, because they cannot be allowed to disagree. Written as two
    /// files, a crash between the writes leaves offsets that describe bytes the
    /// events file doesn't cover, and the next pass re-reads the overlap:
    /// Claude's message-id dedup absorbs that, but Codex events carry no message
    /// id (`agents/codex.rs`), so re-read lines are pushed a second time and the
    /// stale `carry` re-diffs their cumulative snapshots — duplicating tokens as
    /// well as events, silently. One rename means a crash leaves the *previous*
    /// snapshot intact instead, which is merely stale and self-heals.
    fn save_to(&self, dir: &std::path::Path, id: &str) {
        let snap = SnapshotRef {
            version: STORE_VERSION,
            events: &self.events,
            manifest: &self.manifest,
        };
        if let Ok(t) = serde_json::to_string(&snap) {
            let _ = write_atomic(&dir.join(format!("store-{id}.json")), t.as_bytes());
        }
    }

    /// Rebuild the id→index map after the `events` vector is mutated wholesale
    /// (purge/prune shift positions, so partial updates aren't enough).
    fn rebuild_index(&mut self) {
        self.index = self
            .events
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.id.is_empty())
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
    }

    /// Drop every event that came from `key`, then rebuild the index. Used before
    /// re-reading a truncated/rewritten file so re-ingestion is idempotent
    /// (otherwise the cross-line tool_use merge re-appends calls and id-less
    /// events get pushed twice, inflating MCP/Skill counts and token totals).
    fn purge_source(&mut self, key: &str) {
        self.events.retain(|e| e.source != key);
        self.rebuild_index();
    }

    /// Drop events older than `cutoff_ms`. The reports/heatmap only span the last
    /// ~26 weeks, so anything older is dead weight that grows events.json without
    /// bound. Returns whether anything was removed. Old logs already at EOF are
    /// never re-read, so their pruned events don't reappear.
    pub fn prune_before(&mut self, cutoff_ms: i64) -> bool {
        let before = self.events.len();
        self.events.retain(|e| e.ts_ms >= cutoff_ms);
        let removed = self.events.len() != before;
        if removed {
            self.rebuild_index();
        }
        removed
    }

    /// Incrementally read only the new bytes of new/changed JSONL files under
    /// `log_root` (this account's log directory). Returns whether anything
    /// changed (new events or an updated file offset), so the caller can skip a
    /// full cache rewrite when nothing moved.
    pub fn ingest(&mut self, log_root: &std::path::Path, parser: &dyn crate::agents::LogParser) -> bool {
        let mut dirty = false;
        if !log_root.exists() {
            return false;
        }
        for entry in WalkDir::new(log_root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
        {
            let path = entry.path();
            let key = path.to_string_lossy().to_string();
            let Ok(meta) = fs::metadata(path) else { continue };
            let size = meta.len();
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);

            let (offset, carry) = match self.manifest.files.get(&key).cloned() {
                Some(e) => {
                    if e.size == size && e.mtime_ms == mtime_ms {
                        continue; // unchanged → skip
                    }
                    if size < e.offset {
                        // truncated / rewritten (e.g. log compaction): the bytes
                        // we already ingested are gone, so purge this file's
                        // events and re-read from the start, idempotently. The
                        // carried parser state described those bytes, so it goes
                        // too — otherwise the rescan diffs against a stale baseline.
                        self.purge_source(&key);
                        (0, None)
                    } else {
                        (e.offset, e.carry)
                    }
                }
                None => (0, None),
            };
            let mut offset = offset;

            let Ok(mut f) = fs::File::open(path) else { continue };
            if f.seek(SeekFrom::Start(offset)).is_err() {
                continue;
            }
            let mut buf = Vec::new();
            if f.read_to_end(&mut buf).is_err() {
                continue;
            }
            // only process up to the last newline; leave a partial trailing line
            // (file still being written) for the next pass
            let process_until = match buf.iter().rposition(|&b| b == b'\n') {
                Some(i) => i + 1,
                None => 0,
            };
            let mut state = parser.new_file_state(carry.as_ref());
            for line in buf[..process_until].split(|&b| b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                let Ok(s) = std::str::from_utf8(line) else { continue };
                if let Some(mut ev) = state.parse_line(s) {
                    ev.source = key.clone();
                    if !ev.id.is_empty() {
                        if let Some(&i) = self.index.get(&ev.id) {
                            // Same message, another line: merge its tool calls
                            // (don't re-count tokens — usage repeats per line).
                            let prev = &mut self.events[i];
                            prev.mcp.extend(ev.mcp);
                            prev.skills.extend(ev.skills);
                            prev.tools.extend(ev.tools);
                            continue;
                        }
                        self.index.insert(ev.id.clone(), self.events.len());
                    }
                    self.events.push(ev);
                }
            }
            offset += process_until as u64;
            self.manifest.files.insert(
                key,
                FileEntry {
                    size,
                    mtime_ms,
                    offset,
                    carry: state.carry(),
                },
            );
            dirty = true;
        }
        dirty
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{FileState, LogParser};

    /// A parser whose state is a running line count, exposed via carry(). Lets us
    /// assert that state survives a split (incremental) read of the same file.
    struct CountParser;
    struct CountState {
        n: u64,
    }

    impl LogParser for CountParser {
        fn new_file_state(&self, carry: Option<&serde_json::Value>) -> Box<dyn FileState> {
            let n = carry.and_then(|v| v.get("n")).and_then(|v| v.as_u64()).unwrap_or(0);
            Box::new(CountState { n })
        }
    }

    impl FileState for CountState {
        fn parse_line(&mut self, _line: &str) -> Option<RawEvent> {
            self.n += 1;
            None
        }
        fn carry(&self) -> Option<serde_json::Value> {
            Some(serde_json::json!({ "n": self.n }))
        }
    }

    #[test]
    fn carry_survives_an_incremental_read() {
        let dir = std::env::temp_dir().join(format!("ts-carry-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let log = dir.join("a.jsonl");
        fs::write(&log, "one\ntwo\n").unwrap();

        let mut store = Store {
            events: Vec::new(),
            index: HashMap::new(),
            manifest: Manifest::default(),
        };
        store.ingest(&dir, &CountParser);
        let key = log.to_string_lossy().to_string();
        assert_eq!(store.manifest.files[&key].carry, Some(serde_json::json!({ "n": 2 })));

        // Append two more lines; the second pass must resume from 2, not 0.
        fs::write(&log, "one\ntwo\nthree\nfour\n").unwrap();
        store.ingest(&dir, &CountParser);
        assert_eq!(store.manifest.files[&key].carry, Some(serde_json::json!({ "n": 4 })));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_forked_codex_rollout_ingests_as_the_union_of_the_two_files_not_their_sum() {
        // The defect this guards is cross-file, so no per-file invariant can
        // see it: a fork's own file replays its parent's whole token series, and
        // each replayed snapshot legitimately sums into that file's own final
        // cumulative. Only ingesting parent *and* fork together shows it.
        //
        // Parent runs two turns (cumulative input 1000 then 3000); the fork
        // replays both, then runs one of its own (3500). The union is 3500 in /
        // 350 out = 3850 tokens. Counting the replay makes it 7150.
        const P: &str = "019ff518-4ec9-7070-a0bf-955b00458f8c";
        const T1: &str = "019ff518-4ff6-7e72-820b-4c9df55290b9";
        const T2: &str = "019ff535-225e-7960-b555-2be47c5403bc";
        const F: &str = "019ff536-97a5-7f60-8522-ce6613a468da";
        const T3: &str = "019ff536-98c5-76d2-a06a-5bb8f482cfe1";
        let started = |ts: &str, id: &str| format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"task_started","turn_id":"{id}"}}}}"#
        );
        let ctx = |ts: &str| format!(
            r#"{{"timestamp":"{ts}","type":"turn_context","payload":{{"model":"gpt-5.6-sol"}}}}"#
        );
        let tc = |ts: &str, i: u64, o: u64| format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{i},"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":{o}}}}}}}}}"#
        );

        let dir = std::env::temp_dir().join(format!("ts-fork-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let parent = [
            format!(r#"{{"timestamp":"2026-08-12T08:30:38.824Z","type":"session_meta","payload":{{"session_id":"{P}","id":"{P}","cwd":"/w","source":"vscode"}}}}"#),
            started("2026-08-12T08:30:40.000Z", T1),
            ctx("2026-08-12T08:30:40.100Z"),
            tc("2026-08-12T08:30:51.951Z", 1000, 100),
            started("2026-08-12T09:03:30.000Z", T2),
            tc("2026-08-12T09:03:30.915Z", 3000, 300),
        ]
        .join("\n");
        let fork = [
            // The fork's own meta, then the parent's replayed transcript.
            format!(r#"{{"timestamp":"2026-08-12T09:03:43.706Z","type":"session_meta","payload":{{"session_id":"{P}","id":"{F}","forked_from_id":"{P}","cwd":"/w","source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"{P}"}}}}}}}}}}"#),
            format!(r#"{{"timestamp":"2026-08-12T09:03:43.706Z","type":"session_meta","payload":{{"session_id":"{P}","id":"{P}","cwd":"/w","source":"vscode"}}}}"#),
            started("2026-08-12T09:03:43.707Z", T1),
            ctx("2026-08-12T09:03:43.718Z"),
            tc("2026-08-12T09:03:43.718Z", 1000, 100),
            started("2026-08-12T09:03:43.723Z", T2),
            tc("2026-08-12T09:03:43.724Z", 3000, 300),
            // The fork's own first turn.
            started("2026-08-12T09:03:43.847Z", T3),
            ctx("2026-08-12T09:03:47.461Z"),
            tc("2026-08-12T09:03:51.408Z", 3500, 350),
        ]
        .join("\n");
        fs::write(dir.join("parent.jsonl"), parent + "\n").unwrap();
        fs::write(dir.join("fork.jsonl"), fork + "\n").unwrap();

        let mut store = Store {
            events: Vec::new(),
            index: HashMap::new(),
            manifest: Manifest::default(),
        };
        store.ingest(&dir, &*(crate::agents::codex::DESCRIPTOR.parser)());

        let total: f64 = store
            .events
            .iter()
            .map(|e| e.in_tok + e.cc + e.cr + e.out_tok)
            .sum();
        assert_eq!(total, 3850.0, "the union of the two files, not their sum");
        // The fork contributes exactly its own turn, still marked sidechain.
        let sub: Vec<&RawEvent> = store.events.iter().filter(|e| e.sidechain).collect();
        assert_eq!(sub.len(), 1);
        assert_eq!(sub[0].in_tok, 500.0);
        assert_eq!(sub[0].out_tok, 50.0);

        let _ = fs::remove_dir_all(&dir);
    }

    fn codex_shaped_event(ts_ms: i64) -> RawEvent {
        RawEvent {
            ts_ms,
            session: "s1".into(),
            model: "gpt-5.6-sol".into(),
            in_tok: 100.0,
            cc: 0.0,
            cr: 0.0,
            out_tok: 10.0,
            mcp: Vec::new(),
            skills: Vec::new(),
            // Codex events deliberately carry no message id, so nothing dedupes
            // them if the same bytes are ever read twice.
            id: String::new(),
            source: "/logs/a.jsonl".into(),
            cwd: "/w".into(),
            branch: "main".into(),
            tools: Vec::new(),
            sidechain: true,
            tool_results: 0,
            tool_errors: 0,
        }
    }

    #[test]
    fn the_cache_is_one_document_so_events_and_offsets_cannot_skew() {
        let dir = std::env::temp_dir().join(format!("ts-snap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);

        let mut store = Store {
            events: vec![codex_shaped_event(1_000)],
            index: HashMap::new(),
            manifest: Manifest::default(),
        };
        store.manifest.files.insert(
            "/logs/a.jsonl".to_string(),
            FileEntry {
                size: 42,
                mtime_ms: 7,
                offset: 42,
                carry: Some(serde_json::json!({ "prev": { "input": 100.0 } })),
            },
        );
        store.save_to(&dir, "acct");

        // One file, so there is no window in which one artifact is newer than
        // the other. A second file here would mean a crash could skew the pair.
        let mut written: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        written.sort();
        assert_eq!(written, vec!["store-acct.json".to_string()]);

        // Events and offsets come back together, or not at all.
        let back = Store::load_from(&dir, "acct");
        assert_eq!(back.events.len(), 1);
        assert_eq!(back.events[0].out_tok, 10.0);
        assert_eq!(back.manifest.files["/logs/a.jsonl"].offset, 42);
        assert!(back.manifest.files["/logs/a.jsonl"].carry.is_some());

        // A snapshot truncated by a crash is discarded whole: an offset without
        // its events would make ingest() skip the file and lose its history.
        let path = dir.join("store-acct.json");
        let half = fs::read_to_string(&path).unwrap();
        fs::write(&path, &half[..half.len() / 2]).unwrap();
        let broken = Store::load_from(&dir, "acct");
        assert!(broken.events.is_empty());
        assert!(broken.manifest.files.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_snapshot_from_an_older_store_version_is_discarded() {
        let dir = std::env::temp_dir().join(format!("ts-snapver-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        fs::write(
            dir.join("store-acct.json"),
            serde_json::json!({
                "version": STORE_VERSION - 1,
                "events": [],
                "manifest": { "files": { "/logs/a.jsonl": { "size": 1, "mtime_ms": 1, "offset": 1 } } }
            })
            .to_string(),
        )
        .unwrap();

        // Stale offsets must not survive a parser change, or ingest() skips the
        // bytes whose newly-extracted facts the bump exists to pick up.
        let s = Store::load_from(&dir, "acct");
        assert!(s.manifest.files.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncation_clears_carry_and_rereads() {
        let dir = std::env::temp_dir().join(format!("ts-trunc-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let log = dir.join("b.jsonl");
        fs::write(&log, "one\ntwo\nthree\n").unwrap();

        let mut store = Store {
            events: Vec::new(),
            index: HashMap::new(),
            manifest: Manifest::default(),
        };
        store.ingest(&dir, &CountParser);
        let key = log.to_string_lossy().to_string();
        assert_eq!(store.manifest.files[&key].carry, Some(serde_json::json!({ "n": 3 })));

        // Rewrite shorter: the old bytes are gone, so counting restarts at 1.
        fs::write(&log, "x\n").unwrap();
        store.ingest(&dir, &CountParser);
        assert_eq!(store.manifest.files[&key].carry, Some(serde_json::json!({ "n": 1 })));

        let _ = fs::remove_dir_all(&dir);
    }
}
