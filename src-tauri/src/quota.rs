// Claude plan quota. Claude Code persists no quota locally, but its supported
// CLI prints it: `claude -p "/usage"`. We shell out per account rather than
// touching the Keychain token or any undocumented endpoint.
use crate::model::{QuotaSnapshot, QuotaWindow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Parse one `Current <label>: <pct>% used[ · resets <when>]` line.
///
/// Hand-rolled rather than regex: the crate has no regex dependency and this
/// shape is small enough to split. Anything that does not match yields None,
/// so an unrecognised line contributes no window instead of a guessed number.
fn parse_window(line: &str) -> Option<QuotaWindow> {
    let rest = line.trim().strip_prefix("Current ")?;
    let (label, rest) = rest.split_once(": ")?;
    let (pct, rest) = rest.split_once("% used")?;
    let used_percent: f64 = pct.trim().parse().ok()?;

    // Optional " · resets Aug 20 at 12:59am (Asia/Saigon)" tail. The timezone
    // parenthetical is dropped; the rest is shown verbatim. No year is printed,
    // so converting to a unix timestamp would mean guessing one.
    let resets_label = rest
        .split_once("resets ")
        .map(|(_, when)| when.split(" (").next().unwrap_or(when).trim().to_string())
        .unwrap_or_default();

    Some(QuotaWindow {
        label: label.trim().to_string(),
        used_percent,
        resets_at: None,
        resets_label,
    })
}

/// Parse the whole `claude -p "/usage"` stdout. `now_ms` is both `fetched_at`
/// and `source_at`: unlike Codex's log-derived figures, the CLI reports live.
pub fn parse_usage(out: &str, now_ms: i64) -> Option<QuotaSnapshot> {
    let windows: Vec<QuotaWindow> = out.lines().filter_map(parse_window).collect();
    if windows.is_empty() {
        return None;
    }
    let plan = if out.contains("using your subscription") {
        "subscription".to_string()
    } else {
        String::new()
    };
    Some(QuotaSnapshot { plan, windows, fetched_at: now_ms, source_at: now_ms })
}

/// Locate the `claude` binary without a login shell. A GUI app launched at
/// login inherits a minimal PATH, so PATH alone is not enough; the native
/// install's launcher and versioned binaries are checked as fallbacks.
pub fn claude_binary() -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let c = dir.join("claude");
            if c.is_file() {
                return Some(c);
            }
        }
    }
    let home = dirs::home_dir()?;
    let launcher = home.join(".local/bin/claude");
    if launcher.is_file() {
        return Some(launcher);
    }
    // Newest versioned native build, e.g. ~/.local/share/claude/versions/2.1.229
    let versions = home.join(".local/share/claude/versions");
    let found: Vec<PathBuf> = std::fs::read_dir(versions)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    newest_by_version(found)
}

/// Parse a dotted version-like name (e.g. "2.10.229") into numeric components
/// so versions compare by magnitude, not lexicographically — "2.10.0" must
/// outrank "2.9.0", which a plain string sort gets backwards. `None` if any
/// component is not a plain unsigned integer.
fn parse_version(name: &str) -> Option<Vec<u64>> {
    name.split('.').map(|p| p.parse::<u64>().ok()).collect()
}

/// Pick the newest of a set of version-named paths. A name that does not
/// parse as a version sorts as `None`, which is lower than every `Some(_)`,
/// so a stray non-version entry can never be mistaken for the newest build.
fn newest_by_version(paths: Vec<PathBuf>) -> Option<PathBuf> {
    paths.into_iter().max_by_key(|p| {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        parse_version(name)
    })
}

/// Basename of the app-owned scratch directory the `/usage` poll runs in.
///
/// Claude Code names each session log's parent directory after the process's
/// current directory, so running the poll from here makes its log land in
/// `<account>/projects/<slug ending in this name>/` — a directory nobody but
/// Tokenscope could have caused, since nobody runs `claude` by hand from
/// another application's cache. That is what makes the cleanup in
/// `cleanup_probe_logs` safe to point at the user's own data directory.
///
/// The name carries the app's own name even though the directory already sits
/// under `<cache>/tokenscope/`, because only the basename is matched, and it is
/// matched as a *suffix*. A bare "quota-probe" is a name a developer could
/// plausibly give a real project, and any project path ending in it would fall
/// inside the cleanup's scope. Nothing here is meant to be load-bearing alone,
/// but this is the outermost guard and it costs nothing to make it exact.
const PROBE_DIR_NAME: &str = "tokenscope-quota-probe";

/// The scratch directory the poll runs in, created if absent. `None` if the
/// platform cache directory is unavailable, in which case the poll simply runs
/// in whatever directory the app was launched from — a lost cleanup is
/// preferable to a lost quota reading.
fn probe_dir() -> Option<PathBuf> {
    let d = dirs::cache_dir()?.join("tokenscope").join(PROBE_DIR_NAME);
    std::fs::create_dir_all(&d).ok()?;
    Some(d)
}

/// Read one JSONL line as a JSON object. `None` for a blank line or anything
/// that does not parse — callers treat "cannot read" as "unknown content" and
/// therefore as a reason to keep a file, never to delete it.
fn json_line(line: &str) -> Option<serde_json::Value> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    serde_json::from_str(line).ok()
}

/// The text of a `type: "user"` line's message, when it is a plain string.
/// A user line whose content is an array (tool results, images, …) yields
/// `None`: that is real session material, not a slash-command echo.
fn user_text(v: &serde_json::Value) -> Option<&str> {
    if v.get("type").and_then(|t| t.as_str()) != Some("user") {
        return None;
    }
    v.get("message")?.get("content")?.as_str()
}

const USAGE_MARKER: &str = "<command-name>/usage</command-name>";
const CAVEAT_MARKER: &str = "<local-command-caveat>";

/// Is this file one of our `/usage` poll logs?
///
/// True only when the log records the `/usage` command and no assistant turn.
/// Everything else — including a line that fails to parse as JSON — is false,
/// because this predicate gates deleting a file inside the user's Claude data
/// directory and the only acceptable error is keeping a file we wrote.
pub fn is_quota_poll_log(text: &str) -> bool {
    let mut saw_usage = false;
    for line in text.lines() {
        let Some(v) = json_line(line) else {
            if line.trim().is_empty() {
                continue;
            }
            return false;
        };
        if v.get("type").and_then(|t| t.as_str()) == Some("assistant") {
            return false;
        }
        if user_text(&v).is_some_and(|c| c.contains(USAGE_MARKER)) {
            saw_usage = true;
        }
    }
    saw_usage
}

/// Is this file a session that ran `/usage` and recorded nothing else?
///
/// Stricter than `is_quota_poll_log`, because the manual purge walks project
/// directories the user does own. On top of "no assistant turn", every user
/// line must be either the local-command caveat or the `/usage` command echo
/// itself, and any user line whose content is not a plain string (tool results,
/// pasted images) disqualifies the file outright.
///
/// A session where someone typed `/usage` and then did real work has assistant
/// lines and is already excluded by the looser test. A session where someone
/// typed `/usage` and immediately quit is indistinguishable from ours by any
/// means; this last condition is what confines that irreducible ambiguity to
/// sessions that recorded no work at all.
pub fn is_usage_only_session(text: &str) -> bool {
    if !is_quota_poll_log(text) {
        return false;
    }
    for line in text.lines() {
        let Some(v) = json_line(line) else { continue };
        if v.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        let Some(c) = user_text(&v) else { return false };
        if !(c.starts_with(CAVEAT_MARKER) || c.starts_with(USAGE_MARKER)) {
            return false;
        }
    }
    true
}

/// Delete every `.jsonl` file directly inside `dir` that `matches` accepts,
/// returning how many were removed.
///
/// Files only, never directories, never recursive. Every IO error — the
/// directory being unreadable, a file being unreadable, the unlink failing —
/// is swallowed: cleanup is housekeeping and must never cost a caller its
/// result.
fn remove_matching(dir: &Path, matches: fn(&str) -> bool) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let mut removed = 0usize;
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_file() || p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        if matches(&text) && std::fs::remove_file(&p).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Remove the poll's own session logs from one account's `projects` tree.
///
/// Deliberately narrow: only a directory directly under `projects/` whose name
/// *ends with* `PROBE_DIR_NAME`, and within it only `.jsonl` files that pass
/// `is_quota_poll_log`. Claude's path-slug algorithm is never reconstructed —
/// if no such directory exists this does nothing at all.
pub fn cleanup_probe_logs(config_dir: &Path) -> usize {
    let Ok(rd) = std::fs::read_dir(config_dir.join("projects")) else { return 0 };
    let mut removed = 0usize;
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let is_probe = p
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(PROBE_DIR_NAME));
        if is_probe {
            removed += remove_matching(&p, is_quota_poll_log);
        }
    }
    removed
}

/// Purge accumulated quota-check logs across every Claude account, returning
/// how many files were removed.
///
/// The hand-operated fallback for a `cleanup_probe_logs` that silently fails —
/// it swallows every IO error by design, so a persistent failure would pile up
/// files with nothing to say so. This has to reach further than the automatic
/// path: logs written before the scratch directory existed came from a poller
/// with an ordinary working directory, so they sit in ordinary project
/// directories and the basename match will never find them.
///
/// It therefore matches on content instead of location, which is why the
/// predicate is the strict `is_usage_only_session`. Scope is still bounded:
/// one level inside each `projects/<slug>/`, where Claude writes session logs,
/// and never into the nested subdirectories it keeps subagent transcripts in.
pub fn purge_quota_logs() -> usize {
    let mut removed = 0usize;
    for (d, a) in crate::agents::discover_all() {
        if d.id != "claude" {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&a.log_root) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                removed += remove_matching(&p, is_usage_only_session);
            }
        }
    }
    removed
}

/// Run `claude -p "/usage"` for one account and parse the result.
///
/// `config_dir` is the account's data directory, passed as `CLAUDE_CONFIG_DIR`
/// so each account reports its own figures. Returns None on any failure —
/// missing binary, timeout, non-zero exit, or unparseable output — so a bad run
/// never replaces a good cached reading with a wrong one.
///
/// The run's own session log is deleted immediately afterwards, per account and
/// whatever the outcome: a run that timed out or exited non-zero can still have
/// written one, and deferring the sweep to the end of the batch would leave
/// this account's log behind if a later account wedged or the app were killed.
pub fn fetch_claude(config_dir: &Path) -> Option<QuotaSnapshot> {
    let out = run_usage(config_dir);
    cleanup_probe_logs(config_dir);
    parse_usage(&out?, crate::now_ms())
}

/// Spawn `claude -p "/usage"` for one account and return its stdout.
///
/// Runs with its current directory set to the app's scratch directory so the
/// session log Claude Code writes lands somewhere `cleanup_probe_logs` can
/// identify without guessing.
fn run_usage(config_dir: &Path) -> Option<String> {
    let bin = claude_binary()?;
    let mut cmd = std::process::Command::new(bin);
    if let Some(probe) = probe_dir() {
        cmd.current_dir(probe);
    }
    let mut child = cmd
        .env("CLAUDE_CONFIG_DIR", config_dir)
        .arg("-p")
        .arg("/usage")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()
        .ok()?;

    // ~4.5s is typical; 30s is a generous ceiling before we give up and kill it
    // so a wedged child can never pin the poller thread forever.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(_) => {
                // try_wait itself failed; don't leave the child running.
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(out.stdout.as_slice()).into_owned())
}

static CACHE: OnceLock<Mutex<HashMap<String, QuotaSnapshot>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<String, QuotaSnapshot>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn store_cached(account_id: &str, snap: QuotaSnapshot) {
    if let Ok(mut g) = cache().lock() {
        g.insert(account_id.to_string(), snap);
    }
}

pub fn cached(account_id: &str) -> Option<QuotaSnapshot> {
    cache().lock().ok()?.get(account_id).cloned()
}

/// The cache-update decision for one fetch attempt: write only on success.
/// A failed fetch (`None`) leaves whatever was cached before untouched — a
/// stale-but-real reading is recoverable, a wrong one is not, because the
/// user acts on it. An account with nothing cached yet stays absent rather
/// than gaining a placeholder.
fn apply_fetch(account_id: &str, result: Option<QuotaSnapshot>) {
    if let Some(snap) = result {
        store_cached(account_id, snap);
    }
}

/// Refresh every Claude account, one at a time. Sequential on purpose: each run
/// costs several seconds of CPU, and a background menu-bar app should not spawn
/// N of them at once. A failed account keeps whatever was cached before.
///
/// MUST run on a background thread — never the main thread or a BUILD_LOCK
/// holder.
pub fn refresh_claude_accounts() {
    for (d, a) in crate::agents::discover_all() {
        if d.id != "claude" {
            continue;
        }
        // log_root is <data dir>/projects; CLAUDE_CONFIG_DIR wants the data dir.
        let Some(dir) = a.log_root.parent() else { continue };
        apply_fetch(&a.id, fetch_claude(dir));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verbatim output from `claude -p "/usage"` on 2026-08-13.
    const REAL: &str = "You are currently using your subscription to power your Claude Code usage\n\nCurrent session: 15% used · resets Aug 13 at 2:09pm (Asia/Saigon)\nCurrent week (all models): 2% used · resets Aug 20 at 12:59am (Asia/Saigon)\nCurrent week (Fable): 0% used\n\nWhat's contributing to your limits usage?\nLast 24h · 2395 requests · 3 sessions\n";

    #[test]
    fn parses_the_real_three_window_output() {
        let q = parse_usage(REAL, 1_700_000_000_000).expect("a snapshot");
        assert_eq!(q.windows.len(), 3);

        assert_eq!(q.windows[0].label, "session");
        assert_eq!(q.windows[0].used_percent, 15.0);
        assert_eq!(q.windows[0].resets_label, "Aug 13 at 2:09pm");
        // Claude prints no year, so we never invent a machine timestamp.
        assert_eq!(q.windows[0].resets_at, None);

        assert_eq!(q.windows[1].label, "week (all models)");
        assert_eq!(q.windows[1].used_percent, 2.0);
        assert_eq!(q.windows[1].resets_label, "Aug 20 at 12:59am");

        // The third window has no reset clause at all.
        assert_eq!(q.windows[2].label, "week (Fable)");
        assert_eq!(q.windows[2].used_percent, 0.0);
        assert_eq!(q.windows[2].resets_label, "");

        assert_eq!(q.plan, "subscription");
        assert_eq!(q.fetched_at, 1_700_000_000_000);
        assert_eq!(q.source_at, 1_700_000_000_000);
    }

    #[test]
    fn an_unknown_extra_window_is_kept_not_dropped() {
        let out = "Current session: 1% used\nCurrent month (something new): 42% used · resets Sep 1 at 9am (UTC)\n";
        let q = parse_usage(out, 0).unwrap();
        assert_eq!(q.windows.len(), 2);
        assert_eq!(q.windows[1].label, "month (something new)");
        assert_eq!(q.windows[1].used_percent, 42.0);
    }

    #[test]
    fn a_decimal_percentage_parses() {
        let q = parse_usage("Current session: 12.5% used\n", 0).unwrap();
        assert_eq!(q.windows[0].used_percent, 12.5);
    }

    #[test]
    fn output_with_no_recognisable_window_is_none() {
        assert!(parse_usage("", 0).is_none());
        assert!(parse_usage("Login required.\n", 0).is_none());
        assert!(parse_usage("Current session: lots used\n", 0).is_none());
    }

    #[test]
    fn a_non_subscription_preamble_is_reported_as_unknown_plan() {
        let q = parse_usage("Using API credits.\n\nCurrent session: 3% used\n", 0).unwrap();
        assert_eq!(q.plan, "");
        assert_eq!(q.windows.len(), 1);
    }

    #[test]
    fn the_cache_round_trips_a_snapshot() {
        let q = parse_usage(REAL, 42).unwrap();
        store_cached("acct-test", q);
        let got = cached("acct-test").expect("a cached snapshot");
        assert_eq!(got.windows.len(), 3);
        assert_eq!(got.fetched_at, 42);
    }

    #[test]
    fn an_unknown_account_has_no_cached_snapshot() {
        assert!(cached("acct-never-written").is_none());
    }

    #[test]
    fn binary_resolution_prefers_path_then_known_locations() {
        // Whatever this machine has, resolution must be deterministic and must
        // never shell out through a login shell to find it.
        let a = claude_binary();
        let b = claude_binary();
        assert_eq!(a, b);
        if let Some(p) = a {
            assert!(p.is_absolute(), "resolved path must be absolute: {p:?}");
        }
    }

    #[test]
    fn newest_by_version_orders_numerically_not_lexicographically() {
        // A plain string sort ranks "2.9.0" above "2.10.0"; this must not.
        let paths = vec![
            PathBuf::from("/versions/2.9.0"),
            PathBuf::from("/versions/2.10.0"),
            PathBuf::from("/versions/2.2.0"),
        ];
        assert_eq!(newest_by_version(paths), Some(PathBuf::from("/versions/2.10.0")));
    }

    #[test]
    fn newest_by_version_ranks_unparseable_names_lowest() {
        let paths = vec![PathBuf::from("/versions/latest"), PathBuf::from("/versions/2.1.0")];
        assert_eq!(newest_by_version(paths), Some(PathBuf::from("/versions/2.1.0")));
    }

    #[test]
    fn apply_fetch_keeps_a_good_cached_reading_on_failure() {
        let q = parse_usage(REAL, 7).unwrap();
        store_cached("acct-apply-keep", q);
        apply_fetch("acct-apply-keep", None);
        let got = cached("acct-apply-keep").expect("the prior snapshot must remain");
        assert_eq!(got.fetched_at, 7);
    }

    #[test]
    fn apply_fetch_replaces_the_cache_on_success() {
        let old = parse_usage(REAL, 7).unwrap();
        store_cached("acct-apply-replace", old);
        let newer = parse_usage(REAL, 9).unwrap();
        apply_fetch("acct-apply-replace", Some(newer));
        let got = cached("acct-apply-replace").unwrap();
        assert_eq!(got.fetched_at, 9);
    }

    #[test]
    fn apply_fetch_leaves_a_never_cached_account_absent_on_failure() {
        apply_fetch("acct-apply-never", None);
        assert!(cached("acct-apply-never").is_none());
    }

    /// A verbatim-shaped `/usage` poll log: two queue-operation lines, two
    /// hook attachments, the local-command caveat, the `/usage` command echo,
    /// the command's stdout as a system line, and the last-prompt pointer.
    /// Eight lines, no assistant turn — exactly what `claude -p "/usage"`
    /// leaves behind.
    fn poll_log() -> String {
        [
            r#"{"type":"queue-operation","operation":"enqueue","content":"/usage"}"#.to_string(),
            r#"{"type":"queue-operation","operation":"dequeue"}"#.to_string(),
            r#"{"type":"attachment","attachment":{"type":"hook_success"}}"#.to_string(),
            r#"{"type":"attachment","attachment":{"type":"hook_additional_context"}}"#.to_string(),
            format!(
                r#"{{"type":"user","message":{{"role":"user","content":"{CAVEAT_MARKER}Caveat: generated while running local commands."}}}}"#
            ),
            format!(
                r#"{{"type":"user","message":{{"role":"user","content":"{USAGE_MARKER}\n<command-message>usage</command-message>"}}}}"#
            ),
            r#"{"type":"system","subtype":"local_command","content":"Current session: 15% used"}"#
                .to_string(),
            r#"{"type":"last-prompt","leafUuid":"x"}"#.to_string(),
        ]
        .join("\n")
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    /// A scratch `projects` tree with one probe-suffixed directory, named the
    /// way Claude slugs our cache path.
    fn probe_tree(tag: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("tokenscope-quota-test-{tag}"));
        let _ = std::fs::remove_dir_all(&root);
        let probe = root
            .join("projects")
            .join(format!("-Users-someone-Library-Caches-tokenscope-{PROBE_DIR_NAME}"));
        std::fs::create_dir_all(&probe).unwrap();
        (root, probe)
    }

    #[test]
    fn a_genuine_poll_log_is_identified() {
        assert!(is_quota_poll_log(&poll_log()));
    }

    #[test]
    fn a_log_with_an_assistant_line_is_not_ours() {
        let mut t = poll_log();
        t.push_str("\n{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\"}}");
        assert!(!is_quota_poll_log(&t));
    }

    #[test]
    fn a_log_without_the_usage_marker_is_not_ours() {
        let t = r#"{"type":"user","message":{"role":"user","content":"fix the parser"}}"#;
        assert!(!is_quota_poll_log(t));
    }

    #[test]
    fn an_unreadable_line_makes_a_log_not_ours() {
        // Half-written or truncated: we cannot rule out a real turn, so keep it.
        let t = format!("{}\nnot json at all", poll_log());
        assert!(!is_quota_poll_log(&t));
    }

    #[test]
    fn cleanup_removes_only_our_own_poll_logs() {
        let (root, probe) = probe_tree("cleanup");
        let ours = write(&probe, "ours.jsonl", &poll_log());
        let with_assistant = write(
            &probe,
            "real.jsonl",
            &format!(
                "{}\n{}",
                poll_log(),
                r#"{"type":"assistant","message":{"role":"assistant"}}"#
            ),
        );
        let no_marker = write(
            &probe,
            "other.jsonl",
            r#"{"type":"user","message":{"role":"user","content":"hello"}}"#,
        );
        let not_jsonl = write(&probe, "notes.json", &poll_log());

        assert_eq!(cleanup_probe_logs(&root), 1);
        assert!(!ours.exists(), "the poll log must be gone");
        assert!(with_assistant.exists(), "a log with an assistant turn must stay");
        assert!(no_marker.exists(), "a log without the marker must stay");
        assert!(not_jsonl.exists(), "a non-.jsonl file must be ignored");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cleanup_never_touches_a_directory_that_is_not_the_probe() {
        let (root, _probe) = probe_tree("scope");
        let other = root.join("projects").join("-Users-someone-code-myapp");
        std::fs::create_dir_all(&other).unwrap();
        let untouched = write(&other, "session.jsonl", &poll_log());
        assert_eq!(cleanup_probe_logs(&root), 0);
        assert!(untouched.exists(), "only the probe directory is in scope");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_probe_name_is_specific_enough_that_a_users_own_project_cannot_match() {
        // The match is a *suffix* on the directory name, so the basename has to
        // be one nobody would give a real project. A user with a project called
        // "quota-probe" — or anything ending in it — must stay out of scope.
        assert!(PROBE_DIR_NAME.contains("tokenscope"), "the name must be app-specific");
        let (root, _probe) = probe_tree("plausible");
        let mine = root.join("projects").join("-Users-someone-code-quota-probe");
        std::fs::create_dir_all(&mine).unwrap();
        let untouched = write(&mine, "session.jsonl", &poll_log());
        assert_eq!(cleanup_probe_logs(&root), 0);
        assert!(untouched.exists(), "a user's own quota-probe project is not ours");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_probe_directory_slug_still_matches_after_the_rename() {
        // probe_tree names its directory the way Claude slugs the real cache
        // path, so this fails loudly if the constant and the match drift apart.
        let (root, probe) = probe_tree("rename");
        assert!(probe
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(PROBE_DIR_NAME)));
        let ours = write(&probe, "ours.jsonl", &poll_log());
        assert_eq!(cleanup_probe_logs(&root), 1);
        assert!(!ours.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_previous_probe_name_is_still_reached_by_the_new_match() {
        // The scratch directory has always lived at <cache>/tokenscope/<name>,
        // so the old bare "quota-probe" slugged to a name ending in
        // "tokenscope-quota-probe" — which is the new constant. Anything left
        // behind by a build that used the old name is therefore still cleaned
        // up automatically, on any machine. Asserted rather than assumed,
        // because it is a coincidence of the parent directory's name and would
        // stop holding if the cache layout changed.
        let root = std::env::temp_dir().join("tokenscope-quota-test-oldname");
        let _ = std::fs::remove_dir_all(&root);
        let old = root
            .join("projects")
            .join("-Users-someone-Library-Caches-tokenscope-quota-probe");
        std::fs::create_dir_all(&old).unwrap();
        let leftover = write(&old, "leftover.jsonl", &poll_log());
        assert_eq!(cleanup_probe_logs(&root), 1);
        assert!(!leftover.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cleanup_on_a_missing_directory_is_a_silent_no_op() {
        let missing = std::env::temp_dir().join("tokenscope-quota-test-does-not-exist");
        let _ = std::fs::remove_dir_all(&missing);
        assert_eq!(cleanup_probe_logs(&missing), 0);
        // Also the case where projects/ exists but holds no probe directory.
        let (root, _) = probe_tree("empty");
        assert_eq!(cleanup_probe_logs(&root), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_usage_only_session_is_purgeable() {
        assert!(is_usage_only_session(&poll_log()));
    }

    #[test]
    fn a_session_with_usage_plus_real_work_is_kept() {
        // Someone typed /usage and then asked for something. Even before the
        // assistant's reply is written — so the loose test still says "ours" —
        // the extra user turn has to protect it.
        let t = format!(
            "{}\n{}",
            poll_log(),
            r#"{"type":"user","message":{"role":"user","content":"now refactor store.rs"}}"#
        );
        assert!(is_quota_poll_log(&t), "no assistant line yet: the loose test passes");
        assert!(!is_usage_only_session(&t), "the strict test must reject it");
    }

    #[test]
    fn a_session_with_a_structured_user_turn_is_kept() {
        // Array content (tool results, pasted images) is real session material.
        let t = format!(
            "{}\n{}",
            poll_log(),
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#
        );
        assert!(!is_usage_only_session(&t));
    }

    #[test]
    fn the_purge_predicate_still_rejects_what_the_loose_one_does() {
        let with_assistant = format!(
            "{}\n{}",
            poll_log(),
            r#"{"type":"assistant","message":{"role":"assistant"}}"#
        );
        assert!(!is_usage_only_session(&with_assistant));
        assert!(!is_usage_only_session(
            r#"{"type":"user","message":{"role":"user","content":"fix the parser"}}"#
        ));
        assert!(!is_usage_only_session(&format!("{}\nnot json at all", poll_log())));
    }

    #[test]
    fn the_purge_sweeps_ordinary_project_directories_the_probe_match_misses() {
        // The accumulated logs came from a poller with a normal working
        // directory, so they are in a directory `cleanup_probe_logs` ignores.
        let (root, _probe) = probe_tree("purge");
        let ordinary = root.join("projects").join("-Users-someone-code-myapp");
        std::fs::create_dir_all(&ordinary).unwrap();
        let old_poll = write(&ordinary, "old.jsonl", &poll_log());
        let real = write(
            &ordinary,
            "real.jsonl",
            &format!(
                "{}\n{}",
                poll_log(),
                r#"{"type":"user","message":{"role":"user","content":"now refactor store.rs"}}"#
            ),
        );
        assert_eq!(cleanup_probe_logs(&root), 0, "the probe match cannot see these");
        assert_eq!(remove_matching(&ordinary, is_usage_only_session), 1);
        assert!(!old_poll.exists());
        assert!(real.exists(), "a session with real work must survive");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    #[ignore] // spawns `claude` per account; run deliberately, not in CI
    fn live_fetch_each_claude_account() {
        for (d, a) in crate::agents::discover_all() {
            if d.id != "claude" {
                continue;
            }
            let dir = a.log_root.parent().unwrap();
            let q = fetch_claude(dir);
            println!(
                "{} -> {:?}",
                a.label,
                q.map(|x| x
                    .windows
                    .iter()
                    .map(|w| format!("{} {}%", w.label, w.used_percent))
                    .collect::<Vec<_>>())
            );
        }
    }
}

