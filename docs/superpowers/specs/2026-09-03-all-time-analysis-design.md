# Design: all-time analysis page

Date: 2026-09-03
Status: approved for planning

## Goal

Add a dedicated **All time** page to the panel: lifetime totals, month-by-month
history since first use, records, and all-time breakdowns by model / project /
MCP / skill — per account and for the "All" aggregate.

The page must keep meaning as history grows. Today the event store is pruned to
210 days, so summing what is in memory would silently become "the last seven
months" and stop being all time. This design adds a durable per-day rollup that
outlives the prune.

## Research findings

### The 210-day prune is the whole problem

`parser.rs:347` (and again in `build_period`) computes
`cutoff = (now - Duration::days(210)).timestamp_millis()` and hands it to
`Store::prune_before`, which drops every older event and rebuilds the id index.
Those events do not come back: `Store::ingest` skips any file whose size and
mtime match the manifest, so an old log already read to EOF is never re-read.

Retention cannot simply be raised. On the development machine the three
per-account stores are 45 MB, 45 MB and 72 MB for roughly three months of logs.
Removing the cap heads toward gigabyte-scale JSON deserialized in full on every
dashboard build.

### Everything the page needs already exists per-event

`Agg` (parser.rs) already accumulates tokens by type, cost, savings, subagent
tokens, tool results/errors, requests, sessions, and the model / project /
branch / account / tool / mcp / skill maps. The all-time report can reuse it
wholesale rather than growing a second aggregation path.

### Two retroactive behaviours must survive archiving

1. **Whitelist changes apply retroactively.** `compute_event` filters MCP and
   skill names against the *current* `UserConfig` on every build, which is why
   installing an MCP server makes past calls count. Filtering at archive time
   would freeze that.
2. **Price changes apply retroactively.** `compute_event` prices each event from
   the *current* `Pricing` table on every build.

An archive that stored filtered names and a single frozen cost number would
quietly break both.

### Rejected approach: raise or remove the prune

Simplest code, but see the store sizes above. Rejected on memory and load time.

### Rejected approach: sum the store and label it honestly

Ship "All time (since <first event>)" over the ≤210-day window. Fastest, but the
label degrades into a rolling seven-month window the moment history passes the
cap, which is exactly the question the page exists to answer.

## Decisions

1. Persist a **daily rollup** per account, in its own never-pruned file.
2. Archive **unfiltered** MCP / skill / tool names; filter at read time.
3. Archive **raw per-model token components**; re-price at read time.
4. Freeze cost for the project / branch / account attribution maps. Deriving
   them at read time would need a project×model cross product per day; the
   drift after a price change is bounded and small, and is not worth doubling
   the row size. These are the **only** frozen numbers in the archive.
5. Split all-time strictly **by date**: archive rows for days before the raw
   window, live aggregation for days inside it.
6. Rows for days inside the raw window are **rewritten on every build**, never
   appended to.
7. Reuse `PeriodReport` for the body of the report and **wrap** it for the
   extras, rather than defining a parallel report struct.

## Architecture

### Storage: `src-tauri/src/rollup.rs`

One file per account at `~/Library/Caches/tokenscope/rollup-<account-id>.json`,
written with the same atomic temp-file-then-rename used by `store.rs`, and
carrying its own `ROLLUP_VERSION` so a format change discards the archive rather
than misreading it. It is a separate file from `store-<id>.json` on purpose: the
store's events and offset manifest are one consistency unit precisely because an
offset is meaningless without its events, whereas a rollup row is
self-describing and shares no such invariant.

```rust
struct TokBits { input: f64, cc: f64, cr: f64, out: f64, requests: u64 }

struct DayRow {
    date: String,                              // ISO yyyy-mm-dd
    models: HashMap<String, TokBits>,          // RAW model id (price lookup key)
    projects: HashMap<String, (f64, f64)>,     // (tokens, cost) — cost frozen
    branches: HashMap<String, (f64, f64)>,
    accounts: HashMap<String, (f64, f64)>,
    tools: HashMap<String, u64>,               // unfiltered
    mcp: HashMap<String, u64>,                 // unfiltered
    skills: HashMap<String, u64>,              // unfiltered
    hourly: [f64; 24],
    sessions: u64,
    subagent: f64,
    tool_results: u64,
    tool_errors: u64,
}

struct Archive { version: u32, days: BTreeMap<String, DayRow> }
```

Rows are stored generously so the page's content can change without a format
migration. A row is a few kilobytes; five years is under ten megabytes.

Cache savings is deliberately **not** a field: it is re-derivable from each
model's archived `cr` via `Pricing::cache_savings`, and storing it would freeze
a number decision 3 exists to keep live.

Two accumulation rules that differ from the per-model map, mirroring `Agg::add`:
the `tools` / `mcp` / `skills` counts accumulate over **every** event, while
`models` (and therefore `requests` and `sessions`) skip events with an empty
model — the store's marker for a record that is not an LLM request, such as
Claude's slash-command lines and Codex's tool records.

Archives are per-account only. The "All" aggregate is the sum of the per-account
archives at read time and is never itself persisted, so adding or removing an
account cannot leave a stale aggregate behind.

### The invariant

```
all-time = archive rows for days BEFORE the raw window start
         + live aggregation over days INSIDE the raw window
```

"Window start" is the calendar date of the existing prune cutoff —
`(now - 210 days).date_naive()` — so the boundary is a date comparison, matching
how every report already buckets events.

The two sets are disjoint by construction, because the split is on calendar
date, not on which data source happens to hold a day. Every build:

1. aggregates the account's live events per day for days inside the window,
2. **overwrites** the archive rows for exactly those days,
3. reads rows strictly older than the window start from the archive,
4. sums the two.

Consequences, all of them intended:

- No day is counted twice, and no day is missed at the boundary.
- **Backfill is free.** On the first run after upgrade the store already holds up
  to 210 days, so step 2 populates the archive fully on the first build. No
  migration step and no empty page.
- **Price and whitelist changes stay retroactive forever**, not just for 210
  days. That is the point of decisions 2 and 3: names are archived unfiltered
  and per-model tokens are archived raw, so both the whitelist and the price
  table are applied when the archive is *read*. A day archived while offline, or
  before a model had pricing, prices correctly the moment a price table exists —
  no re-archiving needed.
- The bounded exception is decision 4. Project / branch / account **cost** is
  frozen at archive time, so for days outside the raw window those figures keep
  the prices that were current when the day was archived. Their token counts are
  unaffected, and the headline and per-model costs are still re-derived.

What genuinely does not survive is fidelity below one day: an archived day
carries the 24-bucket hour histogram and nothing finer, so the all-time page can
answer "when do I work" but cannot drill into an archived day.

### Sessions

`Agg` counts distinct session ids within a window. A rollup row can only store a
per-day count, so all-time sessions is the sum of daily counts. A session
spanning local midnight is counted in both days. This is rare and the error is
bounded at one per crossing; the alternative is archiving the id set, which is
unbounded. Accepted, and noted in the code.

### Report shape

`Period::All` produces an ordinary `PeriodReport` via the existing `Agg`, so
every existing chart component works untouched. `series` holds **monthly**
buckets across the whole range. The fields that have no meaning at this scale —
`delta_tokens`, `delta_cost`, `trend` — are left at their defaults and are not
rendered; the wrapper's fields replace them instead of faking a comparison.

```rust
struct AllTimeReport {
    report: PeriodReport,
    first: String,                       // ISO of first day with usage ("" if none)
    last: String,
    active_days: u64,
    biggest_day: Option<(String, f64)>,  // (ISO, M tokens)
    longest_streak: u64,
}
```

New Tauri command `get_all_time(account: String) -> AllTimeReport`, registered
alongside `get_period` and run on `spawn_blocking` the same way. It takes
`BUILD_LOCK` like the other builders.

### Frontend

- `data.ts`: `AllTimeReport` interface plus `fetchAllTime(account)`, following
  `fetchPeriod`'s shape including the browser-preview fallback.
- `App.tsx`: `period` state widens to `"Day" | "Week" | "Month" | "All"`.
- Entry point: a fourth item in the existing `Segmented` control. It is the most
  discoverable spot in a 400 px header, and `Segmented` already takes an `items`
  prop, so no new control is needed.
- Selecting `All` swaps the panel body for a new `AllTimePage` component. The
  period navigation (`‹ ›`, Today) and the delta badge are **hidden**, not
  rendered disabled.
- Page content: hero lifetime tokens and cost; month-by-month bars since first
  use; records (biggest day, longest streak, active days); all-time top models,
  projects, MCP servers and skills, reusing `BarList` / `TokenBarList` /
  `CostDonut`.
- The page respects the account tabs: `get_all_time` takes `"all"` or an
  account id exactly as `get_period` does.

## Testing

Rust unit tests in `rollup.rs`, aimed at the failures that would otherwise be
silent:

- A day inside the raw window is **rewritten**, not appended — ingesting twice
  leaves one row with unchanged totals.
- A day outside the window is read from the archive and is not re-derived.
- The two sources never overlap: a synthetic archive whose rows extend past the
  window start still yields each day exactly once.
- A `ROLLUP_VERSION` mismatch discards the archive whole.
- Archived MCP / skill names are filtered by the *current* whitelist at read
  time, so a newly installed server retroactively appears in all-time counts —
  asserted on a row **outside** the raw window, where re-derivation is the only
  thing that can produce the result.
- Archived per-model tokens are re-priced from the current table, likewise
  asserted on a row outside the window.
- `tools` / `mcp` / `skills` counts include events with an empty model, while
  `requests` and `sessions` exclude them — the archive matches `Agg::add`.

Frontend: no new test infrastructure exists in this repo, so the page is
verified by running the app.

## Risks

- **Archive corruption or loss.** Mitigated by the version check and by the fact
  that up to 210 days always rebuild themselves from the raw store. Only history
  older than that is unrecoverable, and it was already unrecoverable today.
- **Cost drift between headline and attribution views** after a price change,
  for days outside the window (decision 4). Bounded and small; revisit only if
  it becomes visible.
- **Rollup write cost on every build.** Rewriting up to 210 rows per account per
  build. Rows are small and the write is one atomic rename; if it shows up in
  profiling, skip the write when no row changed, the way `store.rs` already
  skips a clean save via its `dirty` flag.

## Out of scope

- Changing the 210-day raw retention.
- Per-hour fidelity for archived days beyond the 24-bucket histogram.
- Exporting all-time data.
- Backfilling history older than what the current logs and store hold.
