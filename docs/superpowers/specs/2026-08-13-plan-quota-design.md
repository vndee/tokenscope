# Design: show per-account plan quota

Date: 2026-08-13
Status: approved, ready for implementation planning

## Goal

Checking how much of a plan is left currently means opening the Claude app and
the ChatGPT app separately. Tokenscope already sits in the menu bar and already
knows every account on the machine. Show each account's remaining plan quota
there, so the answer is one click away — and warn from the tray before a limit
is actually hit.

## Research findings

Verified on this machine against real data, not inferred.

### Codex — quota is already in the logs we parse

`event_msg` lines with `payload.type == "token_count"` carry a `rate_limits`
object alongside `info`. **11,080 non-null occurrences** across the corpus:

```json
{"limit_id": "codex", "limit_name": null,
 "primary": {"used_percent": 1.0, "window_minutes": 10080, "resets_at": 1787196735},
 "secondary": null,
 "credits": {"has_credits": false, "unlimited": false, "balance": "0"},
 "individual_limit": null, "spend_control_reached": null,
 "plan_type": "pro", "rate_limit_reached_type": null}
```

Only one shape appears in this corpus: `plan_type: "pro"`, `primary.window_minutes:
10080` (7 days), `secondary: null`. The parser must not assume that — `secondary`
exists in the schema and other plans will differ.

This costs nothing: the Codex adapter already reads these exact lines for token
accounting. An earlier spec listed `rate_limits` under "out of scope"; this
design promotes it.

### Claude — nothing is persisted locally

Checked and ruled out: session JSONL (`message.usage` has tokens and
`service_tier`, no quota), `~/.claude/stats-cache.json` (message/session/tool
counts only, and stale — last written 2026-06-23), `metricsStatusCache` (a bare
timestamp), and `oauthAccount` (`billingType`, `hasExtraUsageEnabled`, but no
consumption). Grepping the logs for rate-limit events returns only the user's
own source code.

The OAuth token lives in the macOS Keychain, not a file.

### Claude — the supported CLI prints exactly what we need

`claude -p "/usage"` returns, on stdout:

```
You are currently using your subscription to power your Claude Code usage

Current session: 15% used · resets Aug 13 at 2:09pm (Asia/Saigon)
Current week (all models): 2% used · resets Aug 20 at 12:59am (Asia/Saigon)
Current week (Fable): 0% used
```

Measured properties:

- **~4.5 s per call** (two runs: 4.4 s, 4.6 s).
- **Consumes no plan quota, but is not free of side effects.** The written log
  contains no `assistant` line and no `message.usage` block, so no tokens, no
  cost and no plan quota are consumed. That is the whole of what was measured:
  whether the CLI makes any network call at all was never observed, so this
  document does not claim it. It *does* write a session log: one new
  `~/.claude/projects/<cwd-slug>/<uuid>.jsonl` per call, 12,413 bytes on
  2026-08-13 with Claude Code 2.1.229. That file carries a fresh `sessionId` and
  a `<command-name>/usage</command-name>` user line, which Tokenscope ingests
  like any other log — so a poller that left them behind would both grow the
  user's data directory and inflate Tokenscope's own session count.

  **Correction.** An earlier revision of this document asserted the opposite
  ("it creates no session log under `~/.claude/projects/`") as a measured fact.
  That measurement used `find … -newermt '-3 minutes'`, GNU-relative syntax that
  BSD `find` on macOS matches nothing for, so it returned an empty result that
  was read as "nothing was written". The claim above was re-measured with a
  plain count instead: 549 `.jsonl` files under `~/.claude/projects` before one
  `claude -p "/usage"`, 550 after, the new file identified with
  `find … -newer <stamp-file>`. Measure side effects with a method that fails
  loudly, and re-measure at the end of the branch.

  **Re-measured at the end of the branch**, with the scratch directory and
  cleanup in place: 1323 `.jsonl` files across both accounts' `projects` trees
  before a two-account fetch, 1323 after, and both
  `projects/-Users-vndee-Library-Caches-tokenscope-tokenscope-quota-probe/`
  directories empty. Plain counts again, not a `find` predicate.
- **Per-account works**: `CLAUDE_CONFIG_DIR=<dir> claude -p "/usage"` returns that
  account's figures. Confirmed distinct across the two accounts here (default
  session 15% / week 2%; work session 5% / week 16%).
- Three windows: a session window, a week window for all models, and a week
  window for a specific premium model. The third line has no reset time.

### Rejected approach: reading the Keychain token

The first framing of this feature assumed reading the OAuth token from the
Keychain and calling an internal endpoint was the only way to get Claude quota.
That was wrong, and the approach is rejected on its merits:

- The CLI path uses a supported public interface and never handles a credential.
- Tokenscope is open-source and distributed via Homebrew, so shipping
  token-reading code would affect every installer, not one machine.
- It would contradict the README's "zero-intrusion, read-only" positioning.

## Decisions

| Question | Decision |
|---|---|
| Claude quota source | `claude -p "/usage"`, per account via `CLAUDE_CONFIG_DIR` |
| Codex quota source | `rate_limits` on `token_count` lines, during existing ingest |
| Credential handling | none, on either path |
| Tray behaviour | warn when quota is high (see constraints below) |

## Architecture

### A common shape

Both adapters produce the same thing, so the UI and the tray never branch on agent:

```rust
pub struct QuotaWindow {
    pub label: String,        // "Session", "Week", "Week (Fable)"
    pub used_percent: f64,
    /// Unix seconds, when the source gives a machine timestamp — Codex does.
    /// `None` for Claude, whose CLI prints only a human string.
    pub resets_at: Option<i64>,
    /// Human reset text for display, e.g. "Aug 20 at 12:59am". Empty if absent.
    pub resets_label: String,
}

pub struct QuotaSnapshot {
    pub plan: String,          // "pro", "subscription", "" when unknown
    pub windows: Vec<QuotaWindow>,
    pub fetched_at: i64,       // when WE observed it
    pub source_at: i64,        // when the DATA was current (see Staleness)
}
```

`AccountData` gains `quota: Option<QuotaSnapshot>` — `None` means "no data",
which the UI must render as absence, never as zero.

### Codex path

The Codex `FileState` already visits every `token_count` line. Capture the
latest non-null `rate_limits` and its timestamp; keep it in `Carry` so it
survives incremental reads. The account's snapshot is the newest across its
files. `source_at` is that event's timestamp.

Map `primary` → one window, `secondary` → a second when present. Do not hardcode
one window.

### Claude path

A poller separate from the ingest loop, because it spawns a process rather than
reading a file:

- runs `CLAUDE_CONFIG_DIR=<account dir> claude -p "/usage"` per Claude account
- **once shortly after launch, then every 60 minutes, plus a Refresh control**
  in the panel's quota block wired to a `refresh_quota` command. The first run
  cannot wait for the interval: the quota cache is in-memory, so an app started
  at login would otherwise show no Claude figure and raise no tray warning for
  its first hour — the exact gap the tick exists to close. Each call writes a
  12 KB session log into the account's own `projects` directory (see the
  measured properties above), so the poll runs with its current directory set to
  `~/Library/Caches/tokenscope/tokenscope-quota-probe` and deletes that log
  immediately afterwards, per account, whatever the run's outcome. At hourly cadence with
  cleanup the steady state on disk is zero files, which is what makes polling
  acceptable at all.

  An earlier revision of this section specified manual-only refresh with no
  timer. That removed the write amplification but broke the tray warning:
  `pick_alert` skips a quota older than 30 minutes, so a figure only a click
  could refresh went quiet between clicks, leaving the 80% alert effectively
  Codex-only. Hourly restores it. The two intervals stay independent, so a
  Claude warning is live for the first half of each hour and quiet for the
  second — a deliberate consequence, not a bug to fix; the panel shows the
  figure with its honest "as of" label throughout.
- **the cleanup is conservative to the point of paranoia**, because it unlinks
  files inside the user's Claude data directory: only a directory directly under
  `projects/` whose name *ends with* `tokenscope-quota-probe` (Claude's slug algorithm is
  never reconstructed — no match means do nothing), only `.jsonl` files directly
  inside it, files only and never directories, and each file must be read and
  found to contain the `/usage` marker with no `"type":"assistant"` line before
  it goes. Every IO error is swallowed: a failed cleanup must never cost a good
  reading.
- **a manual purge** under the plan block covers the case where that silent
  best-effort cleanup persistently fails, and also the logs written before the
  scratch directory existed — those sit in ordinary project directories the
  basename match cannot see. It therefore matches on content with a stricter
  predicate (`/usage` present, no assistant line, *and* no user line other than
  the caveat and the `/usage` echo), and reports how many files it removed.
- sequentially, not in parallel — N accounts × 4.5 s of CPU at once is rude on a
  background menu-bar app. The control therefore holds a disabled, in-flight
  state ("Checking…") for the whole run rather than letting the panel look frozen.
- the cache is in-memory, so a Claude account shows no quota until the first
  poll or manual refresh of that app session
- caches the parsed result; a failed run keeps the previous snapshot and marks it
  stale rather than blanking the display
- `source_at` == `fetched_at`, since the CLI reports live figures

The reset time is kept as text, not converted. Claude prints `Aug 20 at 12:59am
(Asia/Saigon)` — no year — so deriving a unix timestamp means guessing one, and
the guess is wrong around New Year. Codex's `resets_at` is already unix, so it
fills both fields; Claude fills only the label.

Parsing is line-oriented and tolerant: find lines matching
`^Current (.+?): ([0-9]+(?:\.[0-9]+)?)% used(?: · resets (.+?))?$`, capturing
"session" / "week (all models)" / "week (Fable)" as the label. The percentage
allows decimals even though the observed output is integral — Codex reports the
same quantity as a float, and a stricter pattern would silently drop a window if
Claude ever did the same. Anything unrecognised yields no window rather than a
guess. If zero windows parse, the whole snapshot is `None`.

**This is the fragile part.** The output is human-readable text, not an API, and
can change with any Claude Code release. The parser must fail soft, and the
tests must lock the exact current format so a future change fails loudly in CI
rather than silently reporting wrong numbers.

### Staleness

Both sources can be stale, in different ways, and a stale quota shown as current
is worse than no quota — the user will trust it and be surprised.

- Codex `rate_limits` comes from the last logged event. If Codex has not run for
  two days, the figure is two days old.
- The Claude CLI itself qualifies its answer: "Approximate, based on local
  sessions on this machine — does not include other devices or claude.ai."

So every displayed figure carries its `source_at` as a relative label ("as of
2m ago"). Past **30 minutes** the row dims and the tray stops treating it as
actionable. The Claude CLI's own caveat about other devices is surfaced once in
the panel, not per row.

### UI

A compact quota block per account tab, above the existing metrics: one slim bar
per window with its percentage, reset time, and staleness label. Hidden entirely
when `quota` is `None`, following the existing convention that empty sections
disappear rather than render zeros.

### Tray warning

**A constraint to be honest about:** `set_title` on macOS takes plain text and
the system controls its colour, so the *text* cannot be recoloured. What is
available:

- appending a marker to the existing label, e.g. `⬡ 14.00M ⚠`
- swapping the tray icon, since the tray is built with `icon_as_template(false)`
  and therefore renders a coloured image (needs a new amber asset)
- the tooltip, which is the only channel on Windows, where `set_title` is a no-op

The signal is the **highest `used_percent` across all accounts and windows**,
because the useful question is "am I about to be cut off anywhere". Warn at 80%.
A stale snapshot never triggers the warning. The tooltip names which account and
window is high, since the label has no room for it.

## Testing

- Rust unit tests over fixture strings for the `/usage` parser: the exact current
  three-line output; a line with no reset time; an unrecognised line; empty
  output; and output with an extra unknown window (must not break).
- Codex `rate_limits` extraction: `primary` only; `primary` + `secondary`; null
  `rate_limits`; and survival across a `Carry` round-trip.
- Staleness: a snapshot older than the threshold dims and does not trigger the
  tray warning.
- Tray selection: highest percentage across accounts wins; stale entries excluded.
- The poll-log identification predicates, as free functions rather than inlined
  in the delete loop: a genuine poll log, one with an assistant line, one with
  no `/usage` marker, one with an unparseable line, a non-`.jsonl` file, a
  sibling project directory, a missing directory (a silent no-op), and — for the
  stricter purge predicate — a session with `/usage` *plus* real work, which
  must be kept.
- Manual: compare the panel against `claude -p "/usage"` run by hand for both
  accounts, and against the Codex figure in the newest rollout file. Count
  `.jsonl` files under both accounts' `projects` trees before and after a fetch
  (a plain count, never a `find` predicate) and confirm the total is unchanged.

## Risks

- **The `/usage` output format is not a contract.** A Claude Code release can
  change it. Mitigated by fail-soft parsing and format-locking tests, not
  eliminated.
- **Spawning a process on a timer** is heavier than this app's existing work.
  Sequential execution and the 60-minute interval keep it modest, but it is a
  real change in the app's resource profile. (An earlier revision proposed 5
  minutes; at ~288 calls/day/account the session logs it wrote dominated
  Tokenscope's own numbers — 16 of 18 sessions in one day — which is what
  forced both the cleanup and the slower cadence.)
- **The cleanup deletes files inside the user's data directory.** Every guard
  above exists to bound that: the scratch-directory match, the `.jsonl`
  restriction, files-only, and the read-and-verify before each unlink. The
  residual risk is a `/usage`-only session a user started by hand in the
  scratch directory, which nothing can distinguish from ours — and which
  nobody can start, because it is an application cache directory.
- **`claude` must be locatable from the app's environment.** A GUI app launched
  at login does not inherit a shell `PATH`, so resolution is deliberate and
  ordered: `PATH` first, then `~/.local/bin/claude` (the native install's
  launcher on this machine), then the newest `~/.local/share/claude/versions/*`.
  When none resolves, the feature degrades to "no data" — it never shells out
  through a login shell to find it.
- **Codex quota shape is single-sampled.** Only `plan_type: "pro"` with one
  weekly window appears here. Other plans will differ; the parser handles
  `secondary` and missing fields, but no other shape has been observed.

## Out of scope

- Historical quota charts. This shows current state only.
- Notifications or alerts beyond the tray marker.
- Any use of the Keychain token or an undocumented endpoint.
- Quota for agents other than Claude Code and Codex.
