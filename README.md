# Tokenscope

**English** · [中文](README-zh.md)

<a href="https://www.producthunt.com/products/tokenscope-2?embed=true&amp;utm_source=badge-featured&amp;utm_medium=badge&amp;utm_campaign=badge-tokenscope-2" target="_blank" rel="noopener noreferrer"><img alt="Tokenscope - MacOS menu-bar dashboard for Claude CLI token usage | Product Hunt" width="250" height="54" src="https://api.producthunt.com/widgets/embed-image/v1/featured.svg?post_id=1165012&amp;theme=light&amp;t=1780816780292"></a>

A **menu-bar / system-tray app for macOS and Windows** that shows your Claude Code and OpenAI Codex **daily token usage, estimated cost, and per-model / MCP / Skill call breakdown**.

Stack: **Tauri 2 + React + TypeScript** (frontend) / **Rust** (data layer).

![Tokenscope panel (dark / light)](docs/screenshot.png)

## What it does

- Shows today's token count next to the menu-bar icon (e.g. `⬡ 14.00M`)
- Click to open the panel: Day / Week / Month toggle
- Metrics: total tokens (input/output), estimated cost, requests / sessions
- Three breakdowns: **by model** / **by MCP call** / **by Skill call**
- Cost donut (hover for a single model), year-long activity heatmap
- **Counts only the MCP servers / Skills you installed yourself** — built-in tools and vendor-bundled connectors are filtered out (Claude's built-in tools and Anthropic's bundled MCP servers; Codex's built-in `codex_apps` connector); plugin-scoped skills (e.g. `gstack:review`) count too, for both agents
- **Plan usage per account** — how much of each Claude and Codex plan window is spent, with reset times, and a `⚠` on the tray when any window passes 80%, for both agents

## Data sources (zero-intrusion, read-only)

| Purpose | Path |
|---------|------|
| Claude session logs (tokens / model / tool calls) | `~/.claude/projects/**/*.jsonl` |
| Claude MCP whitelist | `~/.claude.json` → `mcpServers` + `projects[*].mcpServers` |
| Claude Skill whitelist | `~/.claude/skills/` directory |
| Codex session logs (tokens / model / tool calls) | `~/.codex/sessions/**/*.jsonl` |
| Codex MCP whitelist | `~/.codex/config.toml` → `[mcp_servers.*]` |
| Codex Skill whitelist | `~/.codex/skills/` and `~/.agents/skills/` |
| Model prices | **Primary**: [models.dev](https://models.dev/api.json) (bare model names, matching Claude CLI / Codex logs) → **Fallback**: [LiteLLM](https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json) → built-in snapshot. Cached in `~/Library/Caches/tokenscope/`, refreshed every 24h, with offline fallback |
| Codex plan quota | `rate_limits` on `token_count` lines in the Codex session logs |
| Claude plan quota | `claude -p "/usage"`, run per account with `CLAUDE_CONFIG_DIR` |
| All-time daily rollup (survives the 210-day event prune) | `~/Library/Caches/tokenscope/rollup-<account>.json` |

Plan quota never touches a credential: Codex reports it inside the logs already
being read, and Claude's comes from its own supported CLI. Tokenscope does not
read the Keychain, any auth file, or any undocumented endpoint. Figures older
than 30 minutes are dimmed rather than shown as current, and the tray's `⚠`
ignores them — so a Claude warning, rechecked hourly, is live for the first half
of each hour and quiet for the second. The panel always shows the figure with
its own "as of" label.

Claude's quota is checked shortly after launch, hourly after that, and whenever
you press **Refresh** in the panel's plan block. Measured on Claude Code 2.1.229, that CLI call consumes no
tokens, no cost and no plan quota — the session log it writes contains no
assistant turn and no `message.usage` block. It does write that log: a ~12 KB
file under the account's own `projects/`, the only thing Tokenscope ever causes
to be written under `~/.claude/`. The check runs from a scratch directory of
Tokenscope's own (`~/Library/Caches/tokenscope/tokenscope-quota-probe`) so the
log lands somewhere identifiable, and deletes it immediately afterwards — a file is
verified to hold the `/usage` command and no assistant reply before it is
removed, and nothing else is ever touched. The steady state is zero accumulated
files. If any are ever left behind, **Clean up leftover check logs** under the
plan block removes them and tells you how many.

Each Skill whitelist directory is scanned two ways: every non-dot top-level directory `<name>/` registers `<name>` (no `SKILL.md` required at that level), and a nested `<plugin>/<name>/SKILL.md` additionally registers `<plugin>:<name>` (a plugin-scoped skill, gated on that `SKILL.md` existing) — so `~/.claude/skills/gstack/review/SKILL.md` and `~/.codex/skills/gstack/review/SKILL.md` both count as `gstack:review` (as well as `gstack` itself, from the top-level scan). Directories starting with `.` are always skipped, at both levels. This applies to both agents' whitelists.

### Key processing
- Deduplicated by `message.id` (streaming/retries repeat the same usage); when one message spans multiple lines, its tool calls are merged and the token usage is counted once
- Token split: `input` (uncached) / `cache` (creation+read) / `output`; the UI folds cache into "In" by default and shows a separate "cached %"
- Price matching: exact id → normalized id (strip vendor prefix + `.`↔`p`, e.g. `glm-5.1`⇄`glm-5p1`); models.dev's official bare-name price wins
- Cost is priced per the four token types; each model carries a `priced` flag — **models not found in either source still count tokens but are labelled "no price" in the UI**
- Logs contain only the bare model name (no vendor) → third-party models default to the official vendor price (an estimate)
- Tool classification (Claude): `mcp__<server>__*` where the server is in your config → MCP; a Skill call (the `Skill` tool's `input.skill`, or a `/skill` slash command) whose name is in your skills whitelist → Skill; everything else is ignored. A plugin-scoped skill is labelled `<plugin>:<skill>`
- Tool classification (Codex): an `mcp_tool_call_end` event names its server → MCP (checked against `config.toml`'s whitelist, so the built-in `codex_apps` connector is filtered out); reading a `skills/<name>/SKILL.md` (or `skills/<plugin>/<name>/SKILL.md`) path during a turn → Skill, once per turn

> Cost is an **estimate** based on public prices; subscription users should read it as "equivalent spend value".

The **All time** page reads a durable per-day rollup instead of the event
store, which keeps only the last 210 days. A day is archived once the raw
store still fully covers it, and rewritten on every launch that ingests new
data until it falls out of that window; the boundary day, which the prune
leaves only partially in the store, is written once and never overwritten by
a later partial read. MCP/Skill names are archived unfiltered and per-model
tokens are archived raw — the whitelist and the price table are applied when
a row is read, not when it's written — so installing an MCP server, adding a
Skill, or a price refresh all still apply retroactively to already-archived
days. The one exception is project / branch / account cost, frozen at
archive time (deriving it per day on read would need a project×model cross
product); their token counts, and the headline and per-model costs, are
still derived fresh on every read. What an archived day cannot do is drill
down below the hour histogram it stores.

### Token types & cost formula

Every assistant message's `usage` reports four **mutually exclusive** token counts (they never double-count the same token):

| Stage | `usage` field | What it is | Price (relative to input) |
|-------|---------------|------------|---------------------------|
| **Input** (uncached) | `input_tokens` | New prompt tokens sent this turn | 1× |
| **Cache write** | `cache_creation_input_tokens` | Context written into the prompt cache | ~1.25× |
| **Cache read** (hit) | `cache_read_input_tokens` | Context replayed from the cache | ~0.1× (much cheaper) |
| **Output** | `output_tokens` | Tokens the model generated | ~5× |

**Tokens** (per period, summed over messages):

```
total  = input + cache_creation + cache_read + output
# the UI shows:  In = input + cache_creation + cache_read,  Out = output,  cached % = cache_read / total
```

**Cost** (each stage priced at its own per-token rate from the price table):

```
cost = input            × price.input
     + cache_creation   × price.cache_creation
     + cache_read       × price.cache_read     # cache hits billed at the discounted read rate
     + output           × price.output
```

So a cache hit is **not** billed as normal input — it uses the dedicated (cheaper) `cache_read` rate, which is why heavily-cached usage shows a huge token count but a modest cost. The UI folds cache into "In" for display only; billing always uses the four separate rates above.

## Install

### Option 1: Homebrew (recommended)

```bash
brew install --cask hdusy/tokenscope/tokenscope
```

The cask's `postflight` strips the quarantine attribute (`xattr -cr`) automatically, so **it opens on first launch without the "Apple cannot verify" prompt**.

After you open it once it registers as a login item, then **launches in the menu bar automatically on every boot**.

Upgrade:

```bash
brew update && brew upgrade --cask tokenscope
```

### Option 2: Download the .dmg

1. Download the latest `Tokenscope_*_universal.dmg` from [Releases](https://github.com/HduSy/tokenscope/releases) (works on both Apple Silicon and Intel)
2. Drag it into Applications
3. Because the build is **unsigned / unnotarized**, Gatekeeper blocks the first launch — pick one:
   - Right-click the app → **Open** → confirm **Open** again, or
   - Run once in the terminal:
     ```bash
     xattr -cr /Applications/Tokenscope.app && open /Applications/Tokenscope.app
     ```

> Unsigned is a current known limitation. A true "double-click to open" experience requires Apple Developer ID signing + notarization — see `PRD.md` §6.4.

### Option 3: Install on Windows

1. Download the latest `Tokenscope_*_x64-setup.exe` from [Releases](https://github.com/HduSy/tokenscope/releases)
2. Double-click to install. Because the build is **unsigned**, Windows SmartScreen will warn on first run — click **More info → Run anyway**
3. The app installs per-user (no admin required) and registers itself for **launch at login** automatically
4. Requirements: **Windows 10 1803+ / Windows 11** with the WebView2 runtime (preinstalled on Windows 11; Windows 10 users without it will be prompted by the installer)

### After first launch

- **macOS**: an icon plus today's token count appears in the menu bar (e.g. `⬡ 12.40M`)
- **Windows**: the tray icon appears in the notification area. The Windows tray API doesn't show a label beside the icon — **hover the tray icon** to see today's token count in the tooltip (e.g. `Tokenscope · today 12.40M`)
- Left-click the icon to toggle the panel; right-click for the menu (Open / Refresh / Quit)
- **Launch-at-login is set up automatically** — no manual configuration needed

## Develop

```bash
pnpm install
pnpm tauri dev         # launch the desktop app (requires the Rust toolchain)
```

Frontend-only preview (using the real-data snapshot `public/dev-dashboard.json`):

```bash
pnpm dev               # http://localhost:1420
# refresh the snapshot:
cd src-tauri && cargo run --example dump > ../public/dev-dashboard.json
```

## Build

```bash
pnpm tauri build       # outputs .app / .dmg on macOS, .exe (NSIS) on Windows to src-tauri/target/release/bundle/
```

For distribution see `PRD.md` §6.3 (Homebrew Cask recommended on macOS; direct `.dmg` / `.exe` downloads benefit from code signing + notarization).

## Structure

```
src/                  React frontend
  data.ts             types + Tauri bridge + theme + formatting
  charts.tsx          chart primitives (bars / donut / sparkline / heatmap / segmented control)
  App.tsx             main panel
src-tauri/src/
  store.rs            incremental JSONL ingest (dedup by message.id + multi-line merge)
  parser.rs           aggregation (Day/Week/Month + heatmap)
  pricing.rs          models.dev / LiteLLM price loading and costing
  config.rs           user MCP / Skill whitelist
  model.rs            data structures returned to the frontend
  lib.rs              Tauri commands + menu-bar tray
  agents/mod.rs       agent adapter registry (where logs live, how to parse them)
  agents/claude.rs    Claude Code discovery + log parsing
  agents/codex.rs     Codex discovery + log parsing (cumulative token deltas)
```

## Bug log

Notable bugs found during development — symptom, root cause, and fix — are
collected in [docs/BUGFIXES.md](docs/BUGFIXES.md).
