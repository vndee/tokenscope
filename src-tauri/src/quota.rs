// Claude plan quota. Claude Code persists no quota locally, but its supported
// CLI prints it: `claude -p "/usage"`. We shell out per account rather than
// touching the Keychain token or any undocumented endpoint.
use crate::model::{QuotaFailKind, QuotaFailure, QuotaSnapshot, QuotaWindow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Month number for Claude's fixed English abbreviation ("Aug"). `None` for
/// anything else, so a locale we have never seen yields no timestamp rather
/// than a wrong month.
fn month_number(abbrev: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS.iter().position(|m| *m == abbrev).map(|i| i as u32 + 1)
}

/// `3:49pm` / `1am` → hour and minute on a 24-hour clock. Claude omits ":00",
/// so both shapes appear in live output. `None` unless the hour is 1–12 and the
/// minute 0–59: 12am is midnight and 12pm is noon, the usual off-by-twelve.
fn parse_clock(s: &str) -> Option<(u32, u32)> {
    let (time, pm) = match (s.strip_suffix("am"), s.strip_suffix("pm")) {
        (Some(t), _) => (t, false),
        (_, Some(t)) => (t, true),
        _ => return None,
    };
    let (h, m) = match time.split_once(':') {
        Some((h, m)) => (h, m.parse::<u32>().ok()?),
        None => (time, 0),
    };
    let h: u32 = h.parse().ok()?;
    if !(1..=12).contains(&h) || m > 59 {
        return None;
    }
    let hour = match (h, pm) {
        (12, false) => 0,
        (12, true) => 12,
        (h, false) => h,
        (h, true) => h + 12,
    };
    Some((hour, m))
}

/// Resolve a reset label — `"Aug 20 at 12:59am"` — to unix seconds.
///
/// The CLI prints no year, which is why this field was left empty until now.
/// The year is recovered rather than guessed: a reset is at most a week from
/// the reading that reports it, and the candidate years sit twelve months
/// apart, so the candidate nearest `now_ms` is the only one that can be
/// intended. Trying the neighbouring years in both directions is what makes
/// the turn of the year work from either side of it.
///
/// Resolved in local time, as the CLI prints it — the parenthetical zone
/// `parse_window` drops always names this machine's own zone, and honouring it
/// would mean carrying a timezone database to reach the same answer.
///
/// `None` whenever the shape is not understood, a candidate year cannot hold
/// the date (Feb 29), or the local time does not exist (the hour a DST jump
/// skips). An ambiguous local time — the hour a DST fall-back repeats — takes
/// the earlier of the two, the one a countdown should not overstate.
fn parse_reset_at(label: &str, now_ms: i64) -> Option<i64> {
    use chrono::{Datelike, Local, LocalResult, TimeZone};

    let (date, time) = label.trim().split_once(" at ")?;
    let (mon, day) = date.trim().split_once(' ')?;
    let month = month_number(mon)?;
    let day: u32 = day.trim().parse().ok()?;
    let (hour, minute) = parse_clock(time.trim())?;

    let now = Local.timestamp_millis_opt(now_ms).single()?;
    let year = now.year();
    [year - 1, year, year + 1]
        .into_iter()
        .filter_map(|y| match Local.with_ymd_and_hms(y, month, day, hour, minute, 0) {
            LocalResult::Single(dt) => Some(dt),
            // A repeated local hour: the earlier reading is the sooner reset.
            LocalResult::Ambiguous(earlier, _) => Some(earlier),
            LocalResult::None => None,
        })
        .map(|dt| dt.timestamp())
        .min_by_key(|secs| (secs - now.timestamp()).abs())
}

/// Parse one `Current <label>: <pct>% used[ · resets <when>]` line.
///
/// Hand-rolled rather than regex: the crate has no regex dependency and this
/// shape is small enough to split. Anything that does not match yields None,
/// so an unrecognised line contributes no window instead of a guessed number.
fn parse_window(line: &str, now_ms: i64) -> Option<QuotaWindow> {
    let rest = line.trim().strip_prefix("Current ")?;
    let (label, rest) = rest.split_once(": ")?;
    let (pct, rest) = rest.split_once("% used")?;
    let used_percent: f64 = pct.trim().parse().ok()?;

    // Optional " · resets Aug 20 at 12:59am (Asia/Saigon)" tail. The timezone
    // parenthetical is dropped; the rest is kept verbatim for display and, when
    // `parse_reset_at` can recover the year the CLI omits, also resolved to a
    // timestamp so the panel can count down to it. The label survives either
    // way: a reset we cannot resolve is still one we can show.
    let resets_label = rest
        .split_once("resets ")
        .map(|(_, when)| when.split(" (").next().unwrap_or(when).trim().to_string())
        .unwrap_or_default();
    let resets_at = parse_reset_at(&resets_label, now_ms);

    Some(QuotaWindow {
        label: label.trim().to_string(),
        used_percent,
        resets_at,
        resets_label,
    })
}

/// Parse the whole `claude -p "/usage"` stdout. `now_ms` is both `fetched_at`
/// and `source_at`: unlike Codex's log-derived figures, the CLI reports live.
pub fn parse_usage(out: &str, now_ms: i64) -> Option<QuotaSnapshot> {
    let windows: Vec<QuotaWindow> =
        out.lines().filter_map(|l| parse_window(l, now_ms)).collect();
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

/// Which failure to report for a run that produced output carrying no usage
/// window. `logged_in` is what an auth probe said, or `None` if it could not
/// say.
///
/// Only an actual answer of "signed out" earns that blame. A probe that itself
/// failed proves nothing, and reporting it as being signed out would send
/// someone to re-authenticate an account that was never the problem.
fn classify_unparsed(logged_in: Option<bool>) -> QuotaFailKind {
    match logged_in {
        Some(false) => QuotaFailKind::SignedOut,
        _ => QuotaFailKind::Unreadable,
    }
}

/// Run `claude -p "/usage"` for one account and parse the result.
///
/// `config_dir` is the account's data directory, passed as `CLAUDE_CONFIG_DIR`
/// so each account reports its own figures — except for the default account,
/// see `scopes_config_dir`. Every failure comes back as a `QuotaFailKind`
/// rather than a bare absence: a bad run must never replace a good cached
/// reading, but it must also not pass for silence.
///
/// Output that parsed to nothing costs one extra process: `claude auth status`,
/// asked only here, on the failure path. It is what separates the single most
/// likely cause — this account is signed out — from every other reason the
/// output might not have carried a figure. Without it the panel can only say
/// that something went wrong, which is what it already said by going quiet.
///
/// The run's own session log is deleted immediately afterwards, per account and
/// whatever the outcome: a run that timed out or exited non-zero can still have
/// written one, and deferring the sweep to the end of the batch would leave
/// this account's log behind if a later account wedged or the app were killed.
pub fn fetch_claude(config_dir: &Path) -> Result<QuotaSnapshot, QuotaFailKind> {
    let out = run_usage(config_dir);
    cleanup_probe_logs(config_dir);
    let out = out?;
    match parse_usage(&out, crate::now_ms()) {
        Some(snap) => Ok(snap),
        None => Err(classify_unparsed(auth_logged_in(config_dir))),
    }
}

/// Ask `claude auth status` whether this account is signed in. `None` when the
/// question could not be answered at all — a missing binary, a wedged run, or
/// output that is not the JSON we know.
///
/// Scoped exactly as the usage run is, and for the same reason: passing
/// `CLAUDE_CONFIG_DIR` for the default account makes Claude Code look for
/// credentials under a scope that does not hold them, and this probe would then
/// report every default account as signed out — turning the one bug this
/// feature exists to expose into the answer it always gives.
fn auth_logged_in(config_dir: &Path) -> Option<bool> {
    // `run.ok` is deliberately ignored: this command exits 1 when signed out
    // and puts the answer on stdout anyway. See `ClaudeRun`.
    let run = run_claude(config_dir, &["auth", "status"]).ok()?;
    parse_auth_logged_in(&run.stdout)
}

/// Read `loggedIn` out of `claude auth status` output. `None` unless the output
/// is JSON carrying that key as a real boolean — a string "true" is not an
/// answer, and neither is a JSON object that simply lacks the field.
fn parse_auth_logged_in(out: &str) -> Option<bool> {
    serde_json::from_str::<serde_json::Value>(out.trim())
        .ok()?
        .get("loggedIn")?
        .as_bool()
}

/// Claude Code's default data directory — where it looks when
/// `CLAUDE_CONFIG_DIR` is unset. `None` when the home directory is unavailable.
fn default_config_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude"))
}

/// Do two paths name the same directory?
///
/// Compares canonicalised forms, so a symlink, a trailing slash or a `..`
/// segment cannot make the default account look like a scoped one. A path that
/// cannot be canonicalised — it may not exist — falls back to its literal form
/// rather than erroring: at worst that keeps the env var, the prior behaviour.
fn same_dir(a: &Path, b: &Path) -> bool {
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    real(a) == real(b)
}

/// Should a poll for `config_dir` carry `CLAUDE_CONFIG_DIR`?
///
/// True for every account except the default one. Claude Code scopes its
/// credential lookup to the directory the variable names, and the default
/// account's credentials are not stored under such a scope — they are stored
/// where an unscoped run finds them. Passing the variable for that account
/// therefore points the lookup at an entry that does not exist and the run
/// comes back logged out, whereupon `/usage` prints an API-style cost summary
/// carrying no percentages at all. `parse_usage` then yields nothing, and
/// because a failed fetch never overwrites the cache, the panel silently keeps
/// showing whatever it last had.
///
/// A second account must still set it: it is the only handle on which account
/// to report, and it works there, because that account's credentials *are*
/// stored under its own scope.
///
/// `default_dir` is `None` when the home directory is unknown; with no default
/// to compare against, keep scoping the run explicitly.
fn scopes_config_dir(config_dir: &Path, default_dir: Option<&Path>) -> bool {
    !matches!(default_dir, Some(d) if same_dir(d, config_dir))
}

/// `claude -p "/usage"` for one account: its stdout, or why there is none.
/// Here a non-zero exit really is a failure — unlike `auth status`, this
/// command has nothing to say through its status code.
fn run_usage(config_dir: &Path) -> Result<String, QuotaFailKind> {
    let run = run_claude(config_dir, &["-p", "/usage"])?;
    if !run.ok {
        return Err(QuotaFailKind::ExitedNonZero);
    }
    Ok(run.stdout)
}

/// One completed `claude` run: what it printed, and whether it exited cleanly.
///
/// The exit status is reported, not judged. `claude auth status` exits non-zero
/// *in order to say* the account is signed out, and prints the answer on stdout
/// while doing it — so treating a non-zero exit as "no answer" would throw away
/// the one reading this whole feature exists to surface.
struct ClaudeRun {
    ok: bool,
    stdout: String,
}

/// ~4.5s is typical for a `/usage` run; this is a generous ceiling before we
/// give up and kill the child, so a wedged one can never pin the poller thread.
const RUN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Run the `claude` binary for one account and return its stdout.
///
/// Every command this module runs goes through here, so they share one account
/// scoping rule and one deadline. Runs with its current directory set to the
/// app's scratch directory, so any session log Claude Code writes lands
/// somewhere `cleanup_probe_logs` can identify without guessing — true of the
/// `/usage` poll, and cheap insurance for anything else.
fn run_claude(config_dir: &Path, args: &[&str]) -> Result<ClaudeRun, QuotaFailKind> {
    let bin = claude_binary().ok_or(QuotaFailKind::NoBinary)?;
    let mut cmd = std::process::Command::new(bin);
    if let Some(probe) = probe_dir() {
        cmd.current_dir(probe);
    }
    // Scope the run to this account, except for the default one — see
    // `scopes_config_dir`. The removal is not merely the absence of the set:
    // the app inherits its environment from whatever launched it, so a
    // variable already present there would otherwise leak into the child.
    if scopes_config_dir(config_dir, default_config_dir().as_deref()) {
        cmd.env("CLAUDE_CONFIG_DIR", config_dir);
    } else {
        cmd.env_remove("CLAUDE_CONFIG_DIR");
    }
    let mut child = cmd
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|_| QuotaFailKind::NoBinary)?;

    let deadline = std::time::Instant::now() + RUN_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(QuotaFailKind::TimedOut);
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(_) => {
                // try_wait itself failed; don't leave the child running.
                let _ = child.kill();
                let _ = child.wait();
                return Err(QuotaFailKind::ExitedNonZero);
            }
        }
    }
    let out = child.wait_with_output().map_err(|_| QuotaFailKind::ExitedNonZero)?;
    Ok(ClaudeRun {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(out.stdout.as_slice()).into_owned(),
    })
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

static FAILURES: OnceLock<Mutex<HashMap<String, QuotaFailure>>> = OnceLock::new();

fn failures() -> &'static Mutex<HashMap<String, QuotaFailure>> {
    FAILURES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Why this account's most recent quota check failed, if it did. `None` once a
/// check has succeeded again.
pub fn last_failure(account_id: &str) -> Option<QuotaFailure> {
    failures().lock().ok()?.get(account_id).copied()
}

/// The cache-update decision for one fetch attempt.
///
/// A good reading is written and ends any standing failure. A failed one leaves
/// the cached figure exactly where it was — a stale-but-real reading is
/// recoverable, a wrong one is not, because the user acts on it — and records
/// why, which is the part that used to be missing. An account with nothing
/// cached yet still gains no placeholder figure; it gains only the reason.
///
/// The two are kept apart on purpose. A stale figure beside a failing check is
/// a real state and the one worth showing: before this, it was indistinguishable
/// from a figure that simply had not changed.
fn apply_fetch(account_id: &str, result: Result<QuotaSnapshot, QuotaFailKind>, now_ms: i64) {
    match result {
        Ok(snap) => {
            store_cached(account_id, snap);
            if let Ok(mut g) = failures().lock() {
                g.remove(account_id);
            }
        }
        Err(kind) => {
            if let Ok(mut g) = failures().lock() {
                g.insert(account_id.to_string(), QuotaFailure { kind, at: now_ms });
            }
        }
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
        apply_fetch(&a.id, fetch_claude(dir), crate::now_ms());
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
        // The year the CLI omits is now recovered, so the label also resolves
        // to a timestamp the panel can count down to; the moments it lands on
        // are pinned in `the_real_output_carries_a_resolved_reset_for_every_window`.
        assert!(q.windows[0].resets_at.is_some());

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

    /// Verbatim stdout from `claude -p "/usage"` on 2026-09-09 in a run whose
    /// credentials did not resolve: the API-style cost summary, no percentages
    /// anywhere. This is the shape the default account started returning once
    /// `CLAUDE_CONFIG_DIR` was passed to it, and the reason the failure was
    /// silent — it parses to nothing rather than to a wrong number.
    const LOGGED_OUT: &str = "Total cost:            $0.0000\nTotal duration (API):  0s\nTotal duration (wall): 2s\nTotal code changes:    0 lines added, 0 lines removed\nUsage:                 0 input, 0 output, 0 cache read, 0 cache write\n";

    #[test]
    fn a_logged_out_run_yields_no_snapshot_rather_than_a_zero() {
        assert!(
            parse_usage(LOGGED_OUT, 0).is_none(),
            "a cost summary carries no quota; reporting 0% used would read as plenty left"
        );
    }

    /// A pair of sibling directories standing in for `~/.claude` and a second
    /// account beside it, both real on disk so `same_dir` can canonicalise them.
    fn account_pair(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("tokenscope-quota-test-{tag}"));
        let _ = std::fs::remove_dir_all(&root);
        let default = root.join(".claude");
        let second = root.join(".claude-work");
        std::fs::create_dir_all(&default).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        (root, default, second)
    }

    #[test]
    fn the_default_account_polls_without_the_scoping_env_var() {
        // The regression this pins: passing CLAUDE_CONFIG_DIR for the default
        // account points Claude Code's credential lookup at a directory-scoped
        // entry that does not exist, and the run comes back logged out.
        let (root, default, _second) = account_pair("scope-default");
        assert!(
            !scopes_config_dir(&default, Some(&default)),
            "the default account must inherit Claude Code's own resolution"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_second_account_still_carries_the_scoping_env_var() {
        // It is the only handle on which account to report, and it works there:
        // a non-default account's credentials are stored under that same scope.
        let (root, default, second) = account_pair("scope-second");
        assert!(scopes_config_dir(&second, Some(&default)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_non_canonical_spelling_of_the_default_still_counts_as_the_default() {
        let (root, default, _second) = account_pair("scope-detour");
        let detour = default.join("..").join(".claude");
        assert!(
            !scopes_config_dir(&detour, Some(&default)),
            "a `..` detour names the same directory"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_the_default_still_counts_as_the_default() {
        let (root, default, _second) = account_pair("scope-symlink");
        let link = root.join("linked");
        std::os::unix::fs::symlink(&default, &link).unwrap();
        assert!(!scopes_config_dir(&link, Some(&default)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unknown_home_leaves_the_env_var_in_place() {
        // Without a home directory there is no default to compare against, so
        // the poll keeps the behaviour it had before: scope the run explicitly.
        assert!(scopes_config_dir(Path::new("/anywhere/.claude"), None));
    }

    #[test]
    fn paths_that_do_not_exist_compare_literally() {
        // canonicalize() fails on a missing path; falling back to the literal
        // form must not collapse two different accounts into one.
        let a = Path::new("/nowhere/tokenscope/.claude");
        let b = Path::new("/nowhere/tokenscope/.claude-work");
        assert!(!scopes_config_dir(a, Some(a)), "identical literals match");
        assert!(scopes_config_dir(b, Some(a)), "different literals do not");
    }

    /// Local-time millis, so every reset test reads in the same zone the CLI
    /// prints in and the parser resolves in — the assertions hold in any zone.
    fn local_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        use chrono::{Local, TimeZone};
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp_millis()
    }

    /// The unix seconds a reset label should resolve to.
    fn local_secs(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        local_ms(y, mo, d, h, mi) / 1000
    }

    #[test]
    fn a_reset_label_resolves_to_the_moment_it_names() {
        let now = local_ms(2026, 9, 9, 15, 0);
        assert_eq!(
            parse_reset_at("Sep 9 at 3:49pm", now),
            Some(local_secs(2026, 9, 9, 15, 49))
        );
    }

    #[test]
    fn a_label_with_no_minutes_resolves_to_the_top_of_the_hour() {
        // Claude omits ":00" — "Sep 10 at 1am", not "Sep 10 at 1:00am".
        let now = local_ms(2026, 9, 9, 15, 0);
        assert_eq!(
            parse_reset_at("Sep 10 at 1am", now),
            Some(local_secs(2026, 9, 10, 1, 0))
        );
    }

    #[test]
    fn noon_and_midnight_are_not_confused_with_each_other() {
        // 12am is hour 0 and 12pm is hour 12; the usual off-by-twelve.
        let now = local_ms(2026, 9, 9, 6, 0);
        assert_eq!(
            parse_reset_at("Sep 9 at 12:30am", now),
            Some(local_secs(2026, 9, 9, 0, 30))
        );
        assert_eq!(
            parse_reset_at("Sep 9 at 12:30pm", now),
            Some(local_secs(2026, 9, 9, 12, 30))
        );
    }

    #[test]
    fn a_reset_just_after_new_year_takes_the_coming_year() {
        // The label carries no year. On Dec 31 a "Jan 1" reset is days away,
        // not a year in the past.
        let now = local_ms(2026, 12, 31, 23, 0);
        assert_eq!(
            parse_reset_at("Jan 1 at 12:59am", now),
            Some(local_secs(2027, 1, 1, 0, 59))
        );
    }

    #[test]
    fn a_reset_just_before_new_year_keeps_the_departing_year() {
        // The mirror case: a reading taken minutes into the new year can still
        // name a moment in the old one.
        let now = local_ms(2027, 1, 1, 0, 30);
        assert_eq!(
            parse_reset_at("Dec 31 at 11:59pm", now),
            Some(local_secs(2026, 12, 31, 23, 59))
        );
    }

    #[test]
    fn a_leap_day_resolves_to_the_only_candidate_year_that_has_one() {
        // Feb 29 exists in 2028 but not in 2027 or 2029; the candidate years
        // that cannot hold the date must be skipped, not guessed at.
        let now = local_ms(2028, 2, 28, 12, 0);
        assert_eq!(
            parse_reset_at("Feb 29 at 3am", now),
            Some(local_secs(2028, 2, 29, 3, 0))
        );
    }

    #[test]
    fn an_unreadable_reset_label_yields_no_timestamp() {
        let now = local_ms(2026, 9, 9, 15, 0);
        for label in ["", "tomorrow", "Sep 9", "Xyz 9 at 1am", "Sep 99 at 1am", "Sep 9 at 25am"] {
            assert!(
                parse_reset_at(label, now).is_none(),
                "{label:?} must not resolve to a guessed moment"
            );
        }
    }

    #[test]
    fn the_real_output_carries_a_resolved_reset_for_every_window() {
        let now = local_ms(2026, 8, 13, 13, 0);
        let q = parse_usage(REAL, now).unwrap();
        assert_eq!(q.windows[0].resets_at, Some(local_secs(2026, 8, 13, 14, 9)));
        assert_eq!(q.windows[1].resets_at, Some(local_secs(2026, 8, 20, 0, 59)));
        // The third window prints no reset at all, so there is nothing to resolve.
        assert_eq!(q.windows[2].resets_label, "");
        assert_eq!(q.windows[2].resets_at, None);
    }

    #[test]
    fn a_window_whose_reset_cannot_be_parsed_still_shows_its_label() {
        // The countdown is an addition, never a reason to lose what the CLI
        // said. An unreadable tail must leave the verbatim text in place.
        let q = parse_usage("Current session: 5% used · resets whenever\n", 0).unwrap();
        assert_eq!(q.windows[0].resets_label, "whenever");
        assert_eq!(q.windows[0].resets_at, None);
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
        apply_fetch("acct-apply-keep", Err(QuotaFailKind::TimedOut), 8);
        let got = cached("acct-apply-keep").expect("the prior snapshot must remain");
        assert_eq!(got.fetched_at, 7);
    }

    #[test]
    fn apply_fetch_replaces_the_cache_on_success() {
        let old = parse_usage(REAL, 7).unwrap();
        store_cached("acct-apply-replace", old);
        let newer = parse_usage(REAL, 9).unwrap();
        apply_fetch("acct-apply-replace", Ok(newer), 9);
        let got = cached("acct-apply-replace").unwrap();
        assert_eq!(got.fetched_at, 9);
    }

    #[test]
    fn apply_fetch_leaves_a_never_cached_account_absent_on_failure() {
        apply_fetch("acct-apply-never", Err(QuotaFailKind::NoBinary), 1);
        assert!(cached("acct-apply-never").is_none());
    }

    /// `claude auth status` output, shape-accurate but with the identifying
    /// fields replaced — a test fixture is no place for a real address or org id.
    const AUTH_IN: &str = r#"{
  "loggedIn": true,
  "authMethod": "claude.ai",
  "apiProvider": "firstParty",
  "analyticsDisabled": false,
  "projectsDirectory": "/Users/someone/.claude/projects",
  "email": "someone@example.com",
  "orgId": "00000000-0000-0000-0000-000000000000",
  "orgName": "Example",
  "subscriptionType": "max"
}"#;

    /// The same command for an account whose credentials did not resolve. Note
    /// what it does *not* carry: no email, no subscription, no auth method.
    const AUTH_OUT: &str = r#"{
  "loggedIn": false,
  "authMethod": "none",
  "apiProvider": "firstParty",
  "analyticsDisabled": false,
  "projectsDirectory": "/Users/someone/.claude/projects"
}"#;

    #[test]
    fn auth_status_tells_a_signed_in_account_from_a_signed_out_one() {
        assert_eq!(parse_auth_logged_in(AUTH_IN), Some(true));
        assert_eq!(parse_auth_logged_in(AUTH_OUT), Some(false));
    }

    #[test]
    fn auth_output_we_cannot_read_says_nothing_either_way() {
        // None means "could not ask", which must never be read as "signed out".
        assert_eq!(parse_auth_logged_in(""), None);
        assert_eq!(parse_auth_logged_in("command not found"), None);
        assert_eq!(parse_auth_logged_in("{}"), None);
        assert_eq!(parse_auth_logged_in(r#"{"loggedIn":"yes"}"#), None);
    }

    #[test]
    fn a_signed_out_cli_is_only_blamed_when_we_actually_checked() {
        // Output with no window is what a signed-out CLI produces, but it is
        // not the only thing that produces it — so the blame needs the answer.
        assert_eq!(classify_unparsed(Some(false)), QuotaFailKind::SignedOut);
        assert_eq!(classify_unparsed(Some(true)), QuotaFailKind::Unreadable);
        assert_eq!(
            classify_unparsed(None),
            QuotaFailKind::Unreadable,
            "an auth probe that itself failed is not evidence of being signed out"
        );
    }

    #[test]
    fn a_failed_fetch_records_why_and_still_keeps_the_last_good_reading() {
        let q = parse_usage(REAL, 7).unwrap();
        store_cached("acct-fail-keep", q);
        apply_fetch("acct-fail-keep", Err(QuotaFailKind::SignedOut), 99);
        assert_eq!(
            cached("acct-fail-keep").expect("the good reading survives").fetched_at,
            7
        );
        let f = last_failure("acct-fail-keep").expect("the reason must be recorded");
        assert_eq!(f.kind, QuotaFailKind::SignedOut);
        assert_eq!(f.at, 99);
    }

    #[test]
    fn a_successful_fetch_ends_an_earlier_failure() {
        apply_fetch("acct-fail-clear", Err(QuotaFailKind::TimedOut), 1);
        assert!(last_failure("acct-fail-clear").is_some());
        apply_fetch("acct-fail-clear", Ok(parse_usage(REAL, 5).unwrap()), 5);
        assert!(
            last_failure("acct-fail-clear").is_none(),
            "a reading that worked is the end of the story, not a second line beside it"
        );
        assert_eq!(cached("acct-fail-clear").unwrap().fetched_at, 5);
    }

    #[test]
    fn an_account_that_never_succeeded_carries_a_reason_and_no_figure() {
        apply_fetch("acct-fail-never", Err(QuotaFailKind::NoBinary), 3);
        assert!(cached("acct-fail-never").is_none(), "nothing may stand in for a figure");
        assert_eq!(last_failure("acct-fail-never").unwrap().kind, QuotaFailKind::NoBinary);
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

    /// A config directory that exists but was never logged into — the exact
    /// state the panel spent hours unable to describe. Verifies against the
    /// real CLI that the auth probe reaches the right conclusion, which no
    /// unit test can do: the classification is pure and covered, but whether
    /// `claude auth status` still answers in the shape we parse is a fact
    /// about the installed binary.
    #[test]
    #[ignore = "spawns the real claude binary"]
    fn live_fetch_blames_a_signed_out_config_dir() {
        let dir = std::env::temp_dir().join("tokenscope-live-signed-out");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let got = fetch_claude(&dir);
        println!("signed-out probe -> {got:?}");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got.err(), Some(QuotaFailKind::SignedOut));
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

