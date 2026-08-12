# Design: track OpenAI Codex alongside Claude Code

Date: 2026-08-12
Status: approved, ready for implementation planning

## Goal

Tokenscope today reads Claude Code session logs only. Add OpenAI Codex CLI as a
second tracked agent — same tokens / cost / model / MCP / Skill breakdowns, same
Day / Week / Month reports, same heatmap — and restructure the ingest layer so a
third agent is a new file rather than a new set of `if` branches.

## Decisions

| Question | Decision |
|---|---|
| How Codex appears in the panel | One more tab in the existing row, with an agent badge |
| Ingest abstraction | Full descriptor/adapter registry under `src-tauri/src/agents/` |
| MCP and Skill tracking for Codex | Both |
| Claude migration | Migrate Claude onto the same adapter in this change, not later |

## Log format research

Every claim below was verified against 66 real session files in
`~/.codex/sessions/` (Codex `0.147.0-alpha.6.6`), not inferred from docs.

### Layout

`~/.codex/sessions/YYYY/MM/DD/rollout-<ISO-ts>-<session-uuid>.jsonl`

One session per file. Each line is `{timestamp, type, payload}` where `type` is
`session_meta` | `event_msg` | `response_item` | `turn_context` | `compacted` |
`world_state` | `inter_agent_communication_metadata`.

### Token accounting

`event_msg` lines with `payload.type == "token_count"` carry:

```json
{"payload": {"type": "token_count", "info": {
  "total_token_usage": {"input_tokens": 27748890, "cached_input_tokens": 26902528,
    "cache_write_input_tokens": 0, "output_tokens": 103557,
    "reasoning_output_tokens": 39350, "total_tokens": 27852447},
  "last_token_usage": { ...same shape, delta for the last request... },
  "model_context_window": 258400}}}
```

Verified properties:

- `total_tokens == input_tokens + output_tokens`.
- `cached_input_tokens` is a **subset** of `input_tokens`, not a sibling.
  Uncached new input is `input_tokens - cached_input_tokens`.
- `reasoning_output_tokens` is a subset of `output_tokens`.
- `total_token_usage` is cumulative per session and **monotonically
  non-decreasing** — checked across the largest session (219 `token_count`
  events, zero decreases), including across two `context_compacted` events.
- Summing `last_token_usage` **over-counts**: 28,402,706 vs a true cumulative of
  27,852,447 in that session, because some `token_count` events repeat the same
  `last_token_usage`.

**Therefore: attribute tokens as the difference between consecutive
`total_token_usage` values**, timestamped at the later event. This is exact,
immune to repeated events, and keeps hour-level resolution for the Day bars.

Components, not `total_tokens`, drive the numbers. One observed session starts at
`total_tokens: 46370` with every component zero (an inherited baseline from a
fork/resume); trusting components correctly attributes nothing to it.

### Model

Two sources, both appearing before the turn's `token_count` events:

- line `type: "turn_context"` (which has no `payload.type`) → `payload.model`
- line `type: "event_msg"`, `payload.type == "thread_settings_applied"` →
  `payload.thread_settings.model`

Observed values: `gpt-5.6-sol`, `codex-auto-review`.

Resolution per `token_count`: the most recently seen model from either source,
else `"unknown"`.

### MCP calls

`event_msg` with `payload.type == "mcp_tool_call_end"`:

```json
{"payload": {"type": "mcp_tool_call_end", "app_name": "...", "connector_id": "...",
  "invocation": {"server": "codex_apps", "tool": "github.get_pr_info", "arguments": {...}}}}
```

`invocation.server` is the server name. All 77 calls in the sample data use
`server: "codex_apps"` — Codex's built-in Apps connector, the structural
equivalent of Anthropic's bundled MCP servers. It is absent from
`~/.codex/config.toml`, so **the existing whitelist rule filters it out with no
special-casing**, exactly as the PRD intends.

Do **not** use `function_call` for MCP. Those are built-in agent tools
(`exec`, `spawn_agent`, `send_message`, `wait_agent`, `list_agents`,
`followup_task`, `wait`), tagged with `payload.namespace` (`collaboration`,
`codex_app`, `plugin_management`).

### Skill invocations

Codex has no skill tool call. A skill is invoked by the agent reading its
`SKILL.md`, which surfaces as `response_item` →
`payload.type == "custom_tool_call"`, `payload.name == "exec"`, with the path in
`payload.input`. Real hits in the sample data:

```
25  /Users/vndee/.agents/skills/review-security
16  /Users/vndee/.agents/skills/review-bugbot
12  ~/.codex/plugins/cache/claude-plugins-official/superpowers/6.2.0/skills/using-superpowers
11  /Users/vndee/.agents/skills/payment-integration
```

Detection is a path match inside the exec input, accepting two shapes:

| Path | Skill id |
|---|---|
| `skills/<name>/SKILL.md` | `<name>` |
| `skills/<plugin>/<name>/SKILL.md` | `<plugin>:<name>` |

A segment starting with `.` is excluded, which keeps the real
`skills/.system/openai-docs/SKILL.md` out. Reference files under a skill —
the real `skills/using-superpowers/references/codex-tools.md` — have no
`SKILL.md` tail and fall out naturally. One exec input can open several skills,
so every match is collected, de-duplicated within the input.

The `<plugin>:<name>` label matters for consistency, not filtering:
`UserConfig::is_user_skill` strips at the last `:` before checking the
whitelist, so either label filters identically — but Claude's parser emits raw
`input.skill` values that are already `plugin:skill`, so both agents' Skill
breakdowns end up labelled the same way. An earlier draft of this spec accepted
only the one-segment shape, which silently dropped the real
`skills/gstack/review/SKILL.md` and put the two agents out of step.

This is a heuristic — it counts "agent opened this skill", which approximates but
does not equal "agent used this skill".

Note also that real `exec` inputs are JS-wrapped
(`tools.exec_command({cmd: "..."})`), not the plain shell strings the samples
above suggest. Matching is plain substring scanning, so the wrapper is harmless.

### Sub-agents

20 of 66 files are sub-agent sessions, identified by `session_meta.source` being
an object (`{"subagent": {...}}`) rather than the string `"vscode"`. A sub-agent
*spawned* fresh has its own `token_count` series from zero, so counting its file
is correct and does not double-count the parent.

A sub-agent that **forked** an existing thread does not. Its `session_meta`
carries `forked_from_id`, and the file opens by replaying the forked-from
thread's whole transcript — `session_meta`, `task_started`, `turn_context`,
`token_count` and `mcp_tool_call_end` — restamped at the fork instant but
carrying the parent's cumulative counters verbatim. Diffing those from a zero
baseline re-counts the parent's entire history as fresh usage, at the wrong
hour and day: measured over one day of real logs, 102,682,825 of 576,858,003
tokens ($77.91 of $437.02) and 106 of 143 MCP calls.

The replayed prefix must therefore establish the fork's baseline and contribute
nothing. Turn ids and thread ids are UUIDv7, so the boundary is exact rather
than a timing guess: a turn minted before this thread's own id existed belongs
to the thread it forked from. The first turn minted at or after the fork ends
the replay for good (`agents/codex.rs`).

### No overlap with Claude data

Codex logs contain only `gpt-5.6-sol` and `codex-auto-review`. Despite
`~/.codex/claude-cowork-import-history.json` existing, no imported Claude session
carries Anthropic token counts into `~/.codex/sessions/`. No double-counting risk.

### Pricing

`gpt-5.6-sol` is on models.dev (`input 5`, `output 30`, `cache_read 0.5`,
`cache_write 6.25` per 1M). `pricing.rs::normalize_key` maps both the log id and
the models.dev id to `gpt-5p6-sol`, so it matches with no changes.

`codex-auto-review` is not on models.dev and stays `priced: false`, which the UI
already renders as unknown rather than $0.

Known pre-existing limitation, unchanged here: `ModelPrice` is flat, so
models.dev's >272k-context tier pricing is not applied.

## Architecture

### New module `src-tauri/src/agents/`

```
agents/mod.rs      AgentDescriptor, AccountSpec, LogParser/FileState traits, registry()
agents/claude.rs   Claude Code — discovery moved from accounts.rs, parsing from store.rs
agents/codex.rs    Codex — new
```

Declarative half — each agent states where its data lives:

```rust
pub struct AccountSpec {
    pub id: String,             // slug of the config dir; namespaces the cache and tab key
    pub agent: &'static str,    // "claude" | "codex"
    pub label: String,
    pub email: String,
    pub log_root: PathBuf,      // walked for *.jsonl
    pub config_file: PathBuf,
    pub skill_dirs: Vec<PathBuf>,
}

pub struct AgentDescriptor {
    pub id: &'static str,
    pub display: &'static str,
    pub discover: fn() -> Vec<AccountSpec>,
    pub load_config: fn(&AccountSpec) -> UserConfig,
    pub parser: fn() -> Box<dyn LogParser>,
}

pub fn registry() -> &'static [AgentDescriptor];  // [claude::DESCRIPTOR, codex::DESCRIPTOR]
```

Behavioural half — parsing is per-file and stateful, because Codex needs the
previous cumulative counter and the current model:

```rust
pub trait LogParser: Send + Sync {
    /// One state object per source file. `carry` is the state persisted by the
    /// previous incremental pass over this same file, or None when reading from
    /// byte 0.
    fn new_file_state(&self, carry: Option<&serde_json::Value>) -> Box<dyn FileState>;
}

pub trait FileState {
    fn parse_line(&mut self, line: &str) -> Option<RawEvent>;
    /// Streaming state to persist in the manifest so the next pass resumes exactly.
    fn carry(&self) -> Option<serde_json::Value> { None }
}
```

`store.rs` keeps the byte-offset walk, dedup/merge, pruning and cache I/O, and
loses every Claude-specific line. Claude's `parse_line` / `parse_assistant` /
`parse_user_command` move verbatim into `agents/claude.rs` behind a stateless
`FileState`.

### Store version 6 → 7

`Manifest` currently stores `path -> (size, mtime_ms, offset)`. Codex's
cumulative-diff parsing needs the last cumulative counter to survive between
incremental passes; without it, the first `token_count` of each new read would
diff against zero and re-add the whole session total.

```rust
#[derive(Serialize, Deserialize, Clone, Default)]
struct FileEntry {
    size: u64,
    mtime_ms: i64,
    offset: u64,
    #[serde(default)]
    carry: Option<serde_json::Value>,
}
```

Bump `STORE_VERSION` to 7 so existing caches are discarded and rescanned once.
The truncation path (`size < offset`) must clear `carry` along with purging the
file's events, or the rescan diffs against a stale baseline.

### Watcher

`lib.rs` watches each account's `<data_dir>/projects` for sub-second refresh. It
must iterate `AccountSpec.log_root` instead, so Codex's `sessions/` tree is
watched on the same path.

### Codex file state

```rust
struct CodexState {
    session: String,          // session_meta.session_id
    model: String,            // latest turn_context / thread_settings_applied
    prev: Option<CumTokens>,  // last total_token_usage seen
    turn_skills: HashSet<String>,  // skills already counted this turn
}
```

Per line:

- `session_meta` → record `session_id`.
- `turn_context` / `thread_settings_applied` → update `model`; clear `turn_skills`.
- `task_started` → clear `turn_skills`.
- `token_count` → emit a `RawEvent` from the component-wise delta against `prev`,
  then store the new cumulative as `prev`. Skip when every delta is zero.
- `mcp_tool_call_end` → emit a zero-token `RawEvent` with
  `mcp: vec![invocation.server]`.
- `custom_tool_call` named `exec` whose input matches a skill path (see above)
  and whose `<name>` is not already in `turn_skills` → emit a zero-token
  `RawEvent` with `skills: vec![name]`, and record it.

Zero-token events carrying only a tool name follow the precedent already set by
Claude's slash-command path (`parse_user_command`), and `parser.rs` aggregates
them correctly today.

Field mapping into `RawEvent`:

| `RawEvent` | Codex delta |
|---|---|
| `in_tok` | Δ`input_tokens` − Δ`cached_input_tokens` |
| `cr` | Δ`cached_input_tokens` |
| `cc` | Δ`cache_write_input_tokens` |
| `out_tok` | Δ`output_tokens` |
| `ts_ms` | line `timestamp` (RFC3339, UTC) |
| `session` | `session_meta.session_id` |
| `model` | resolved model, else `"unknown"` |
| `id` | empty — the byte-offset manifest already guarantees one read per line |
| `cwd` | `session_meta.payload.cwd` |
| `branch` | `session_meta.payload.git.branch` (may be null) |
| `sidechain` | latches true once any `session_meta.payload.source` is an object with a `subagent` key |
| `tools` | every `function_call` / `custom_tool_call` name, plus `<server>.<tool>` from `mcp_tool_call_end` |
| `tool_results`, `tool_errors` | left 0 — see below |

`RawEvent` grew past what the earlier draft of this spec assumed; the table above
reflects the fields as of `STORE_VERSION` 6.

Codex has **no general tool-error flag**. `custom_tool_call_output` /
`function_call_output` carry free text ("Script completed / Wall time / Output:")
with no `is_error` equivalent, and `exec` failures — the bulk of tool calls —
are indistinguishable from successes. Only `patch_apply_end` exposes a `success`
boolean. Reporting reliability from patch applications alone would understate the
true error rate, so both counters stay 0 for Codex and the reliability panel
shows no data on a Codex tab. This must be verified to render as "no data"
rather than a misleading 0%.

### Codex discovery and whitelist

Accounts: `~/.codex` (default), `$CODEX_HOME` when set, then sibling `~/.codex*`
directories, mirroring `accounts::discover`. A directory qualifies only if
`sessions/` exists. Default label `"Codex"`; the tab is renameable already, and
`auth.json` is deliberately not read because it holds refresh tokens.

MCP whitelist: `~/.codex/config.toml` → top-level `[mcp_servers.<name>]` keys.
Adds a `toml` dependency. Feeds the existing `UserConfig::is_user_mcp` unchanged.

Skill whitelist: directory names under `<codex-dir>/skills/` and `~/.agents/skills/`,
feeding `UserConfig::is_user_skill` unchanged.

### Frontend

- `AccountData` gains `agent: "claude" | "codex"`; `data.ts` type updated.
- `AccountTabs` renders a small per-agent badge so the two CLIs are
  distinguishable at a glance without a second nav level.
- `parser.rs::vendor_of` gains a `codex` → OpenAI branch, so
  `codex-auto-review` is not bucketed as "Other".

No change is needed to hide empty MCP/Skill panels. `App.tsx` already gates both
sections on `M.servers > 0` / `M.skills > 0` — the size of the *whitelist*, not
the call list. A Codex account with no configured servers and no skill dirs
hides both automatically; one with servers configured but no user-MCP calls
shows the existing "No MCP calls in this period" line, which is accurate.
Re-gating on the call list would instead suppress that message for Claude users,
so the existing behaviour stands.

## Testing

- Rust unit tests in `agents/codex.rs` over small fixture JSONL strings:
  cumulative diff across several `token_count` events; repeated
  `last_token_usage` not inflating totals; cumulative carried across a simulated
  split read (parse first half, persist `carry`, resume); model resolution order;
  `mcp_tool_call_end` extraction; skill path extraction and per-turn dedup;
  zero-component baseline attributing nothing.
- A regression check that Claude ingestion produces byte-identical `RawEvent`s
  before and after the adapter move — run the existing `examples/dump` against a
  fixed log set on both revisions and diff.
- Manual: `cargo run --example dump` with real `~/.codex` data, confirm the Codex
  tab totals, then cross-check one session's total against the last
  `total_token_usage` in its file.

## Risks and accepted limitations

- **Skill detection is a heuristic.** It counts SKILL.md reads. A skill read but
  not followed, or re-read mid-session, skews the count. Per-turn dedup limits
  the damage.
- **Cost is an API-rate estimate.** The user is on a ChatGPT subscription, so the
  dollar figure is notional. This matches how the app already treats Claude Max
  usage, so the two tabs stay comparable.
- **`gpt-5.6-*` are new models.** If models.dev drops or renames them the cost
  silently falls back to `priced: false`; the UI already signals unknown pricing.
- **Log format is undocumented and pre-1.0** (`0.147.0-alpha`). Field renames in
  a future Codex release would break ingestion. Parsing is defensive: an
  unrecognised line yields `None` rather than an error.

## Out of scope

- Reading `~/.codex/logs_2.sqlite` or `state_5.sqlite` for richer telemetry.
- `rate_limits` from `token_count` payloads (plan-quota display).
- `model_context_window` / context-usage display.
- Agents beyond Claude and Codex; the registry makes them additive.
