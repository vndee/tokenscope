// Shared data structures returned to the frontend.
use serde::Serialize;

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
