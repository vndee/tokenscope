// Claude plan quota. Claude Code persists no quota locally, but its supported
// CLI prints it: `claude -p "/usage"`. We shell out per account rather than
// touching the Keychain token or any undocumented endpoint.
use crate::model::{QuotaSnapshot, QuotaWindow};

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
}
