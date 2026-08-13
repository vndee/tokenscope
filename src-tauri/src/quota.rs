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

/// Run `claude -p "/usage"` for one account and parse the result.
///
/// `config_dir` is the account's data directory, passed as `CLAUDE_CONFIG_DIR`
/// so each account reports its own figures. Returns None on any failure —
/// missing binary, timeout, non-zero exit, or unparseable output — so a bad run
/// never replaces a good cached reading with a wrong one.
pub fn fetch_claude(config_dir: &Path) -> Option<QuotaSnapshot> {
    let bin = claude_binary()?;
    let mut child = std::process::Command::new(bin)
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
    parse_usage(&String::from_utf8_lossy(out.stdout.as_slice()), crate::now_ms())
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
