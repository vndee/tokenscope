// Shared data structures returned to the frontend.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct SeriesPoint {
    pub label: String, // sparse axis label (many empty)
    pub full: String,  // complete label for the hover tooltip (hour / date)
    pub input: f64,    // M tokens (uncached new input)
    pub cache: f64,    // M tokens (cache creation + read)
    pub output: f64,   // M tokens
    // ISO yyyy-mm-dd this bar represents, for click-to-drill-down. Empty for the
    // Day report's hourly bars (an hour isn't a drillable calendar day).
    pub date: String,
}

/// One point on the zoomed-out trend line: a whole day/week/month total.
#[derive(Debug, Clone, Serialize)]
pub struct TrendPoint {
    pub label: String,  // sparse x-axis tick (many empty)
    pub full: String,   // full label for the hover tooltip
    pub tokens: f64,    // M tokens for the whole period
    pub cost: f64,      // USD estimate for the whole period
    pub date: String,   // ISO yyyy-mm-dd anchor of the period (for click-to-view)
    pub current: bool,  // the period currently being viewed (highlighted)
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelStat {
    pub name: String,
    pub vendor: String,
    pub tokens: f64, // M tokens (input+output, weighted)
    pub cost: f64,   // USD estimate
    pub color: String,
    pub priced: bool, // false = no pricing data in LiteLLM (cost is unknown, not $0)
}

#[derive(Debug, Clone, Serialize)]
pub struct NamedCount {
    pub name: String,
    pub count: u64,
}

/// A named token/cost bucket (project, branch, …): tokens in M, cost in USD.
#[derive(Debug, Clone, Serialize)]
pub struct NamedTokens {
    pub name: String,
    pub tokens: f64,
    pub cost: f64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Metrics {
    #[serde(rename = "totalTokens")]
    pub total_tokens: f64,
    #[serde(rename = "inputTokens")]
    pub input_tokens: f64,
    #[serde(rename = "cacheTokens")]
    pub cache_tokens: f64,
    #[serde(rename = "outputTokens")]
    pub output_tokens: f64,
    pub cost: f64,
    #[serde(rename = "cacheSavings")]
    pub cache_savings: f64, // USD saved by cache reads this period
    #[serde(rename = "subagentTokens")]
    pub subagent_tokens: f64, // M tokens spent inside subagents (isSidechain)
    #[serde(rename = "toolResults")]
    pub tool_results: u64, // tool_result blocks seen (reliability denominator)
    #[serde(rename = "toolErrors")]
    pub tool_errors: u64, // of those, how many were is_error
    #[serde(rename = "mcpCalls")]
    pub mcp_calls: u64,
    #[serde(rename = "skillCalls")]
    pub skill_calls: u64,
    pub requests: u64,
    pub sessions: u64,
    #[serde(rename = "deltaTokens")]
    pub delta_tokens: f64,
    #[serde(rename = "deltaCost")]
    pub delta_cost: f64,
    pub servers: u64,
    pub skills: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PeriodReport {
    pub metrics: Metrics,
    pub series: Vec<SeriesPoint>,
    pub models: Vec<ModelStat>,
    // Token/cost attribution by project (cwd basename) and git branch, plus the
    // full tool-usage counts (built-in tools, mcp__ excluded — MCP has its own view).
    pub projects: Vec<NamedTokens>,
    pub branches: Vec<NamedTokens>,
    // Per-account token/cost split; only meaningful (>1 entry) in the "All"
    // aggregate report, where the frontend shows it.
    pub accounts: Vec<NamedTokens>,
    pub tools: Vec<NamedCount>,
    pub mcp: Vec<NamedCount>,
    pub skills: Vec<NamedCount>,
    #[serde(rename = "reqTrend")]
    pub req_trend: Vec<f64>,
    #[serde(rename = "costTrend")]
    pub cost_trend: Vec<f64>,
    // Hour-of-day token histogram (24 buckets, M tokens) across this period's
    // events — powers the "most active hours" insight for any period.
    pub hourly: Vec<f64>,
    // Human label of the period being shown (e.g. "Wed, Jul 3",
    // "Jun 30 – Jul 6", "Jul 2026") for the navigation bar.
    pub range: String,
    // Zoomed-out trend: last 14 days (Day) / 12 weeks (Week) / 6 months (Month),
    // ending at this report's period. Points are click-to-view.
    pub trend: Vec<TrendPoint>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HeatDay {
    pub date: String, // ISO yyyy-mm-dd
    pub tokens: f64,  // M tokens
    pub level: u8,    // 0..4
}

#[derive(Debug, Clone, Serialize)]
pub struct Dashboard {
    pub day: PeriodReport,
    pub week: PeriodReport,
    pub month: PeriodReport,
    pub heatmap: Vec<HeatDay>,
    #[serde(rename = "todayTokens")]
    pub today_tokens: f64, // M tokens, for the tray label
    #[serde(rename = "generatedAt")]
    pub generated_at: String,
}

/// One Claude CLI account (= one config directory) plus its own dashboard.
#[derive(Debug, Clone, Serialize)]
pub struct AccountData {
    pub id: String,    // stable key (slug of the config dir), also the tab key
    pub label: String, // friendly name (org / display name / email / dir)
    pub email: String, // account email if known (may be empty)
    pub agent: String, // owning CLI ("claude" / "codex") — drives the tab badge
    /// Plan quota for this account, when known. `None` renders as absence — the
    /// UI must never show it as zero.
    pub quota: Option<QuotaSnapshot>,
    pub dash: Dashboard,
}

/// Everything the panel needs in one fetch: each account's dashboard plus an
/// aggregate ("All") that sums every account. `today_tokens` is the combined
/// figure shown next to the tray icon.
#[derive(Debug, Clone, Serialize)]
pub struct Workspace {
    pub accounts: Vec<AccountData>,
    pub all: Dashboard,
    #[serde(rename = "todayTokens")]
    pub today_tokens: f64,
}

/// One rolling limit window on a plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub label: String, // "5h" / "Week" / "Session" / "Week (all models)"
    #[serde(rename = "usedPercent")]
    pub used_percent: f64,
    /// Unix seconds, when the source gives a machine timestamp — Codex does.
    /// None for Claude, whose CLI prints only a human string with no year.
    #[serde(rename = "resetsAt")]
    pub resets_at: Option<i64>,
    /// Human reset text for display, e.g. "Aug 20 at 12:59am". Empty if absent.
    #[serde(rename = "resetsLabel", default)]
    pub resets_label: String,
}

/// An account's plan quota as of a point in time. `source_at` is when the data
/// was true; `fetched_at` is when we observed it. They differ for Codex, whose
/// figures come from the last logged event and can be days old.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    pub plan: String,
    pub windows: Vec<QuotaWindow>,
    #[serde(rename = "fetchedAt")]
    pub fetched_at: i64,
    #[serde(rename = "sourceAt")]
    pub source_at: i64,
}
