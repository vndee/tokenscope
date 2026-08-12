import { invoke } from "@tauri-apps/api/core";

export interface SeriesPoint { label: string; full: string; input: number; cache: number; output: number; date: string }
export interface ModelStat { name: string; vendor: string; tokens: number; cost: number; color: string; priced: boolean }
export interface NamedCount { name: string; count: number }
export interface NamedTokens { name: string; tokens: number; cost: number }
// One point on the zoomed-out trend line (a whole day/week/month total).
export interface TrendPoint { label: string; full: string; tokens: number; cost: number; date: string; current: boolean }
export interface Metrics {
  totalTokens: number; inputTokens: number; cacheTokens: number; outputTokens: number; cost: number;
  cacheSavings: number; subagentTokens: number; toolResults: number; toolErrors: number;
  mcpCalls: number; skillCalls: number; requests: number; sessions: number;
  deltaTokens: number; deltaCost: number; servers: number; skills: number;
}
export interface PeriodReport {
  metrics: Metrics; series: SeriesPoint[]; models: ModelStat[];
  projects: NamedTokens[]; branches: NamedTokens[]; accounts: NamedTokens[]; tools: NamedCount[];
  mcp: NamedCount[]; skills: NamedCount[]; reqTrend: number[]; costTrend: number[];
  hourly: number[];
  range: string; trend: TrendPoint[];
}
export interface HeatDay { date: string; tokens: number; level: number }
export interface Dashboard {
  day: PeriodReport; week: PeriodReport; month: PeriodReport;
  heatmap: HeatDay[]; todayTokens: number; generatedAt: string;
}
// One tracked account (= one agent CLI config dir) and its dashboard.
export interface AccountData { id: string; label: string; email: string; agent: string; dash: Dashboard }
// Every account plus an aggregate "All"; todayTokens is the combined tray total.
export interface Workspace { accounts: AccountData[]; all: Dashboard; todayTokens: number }

export async function fetchWorkspace(): Promise<Workspace> {
  // Inside the Tauri runtime → call the Rust backend.
  const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
  if (inTauri) return invoke<Workspace>("get_workspace");
  // Browser dev/preview fallback → static single-account snapshot of real data.
  const res = await fetch("/dev-dashboard.json");
  if (!res.ok) throw new Error("not running in Tauri and no dev snapshot found");
  const dash: Dashboard = await res.json();
  return { accounts: [{ id: "dev", label: "Dev", email: "", agent: "claude", dash }], all: dash, todayTokens: dash.todayTokens };
}

// Fetch one period report for a specific account ("all" or an id) + reference
// date (ISO), for date navigation / drill-down into past periods.
export async function fetchPeriod(account: string, period: string, reference: string): Promise<PeriodReport> {
  const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
  if (inTauri) return invoke<PeriodReport>("get_period", { account, period, reference });
  // Dev fallback: just return the snapshot's matching current period.
  const res = await fetch("/dev-dashboard.json");
  if (!res.ok) throw new Error("no dev snapshot");
  const dash: Dashboard = await res.json();
  return period === "Day" ? dash.day : period === "Month" ? dash.month : dash.week;
}

// ── date navigation helpers (local time) ───────────────────────────
const pad2 = (n: number) => String(n).padStart(2, "0");
export const fmtISO = (d: Date) => `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;
export const todayISO = () => fmtISO(new Date());
const isoToDate = (iso: string) => new Date(iso + "T00:00:00");
export function weekStartISO(iso: string): string {
  const d = isoToDate(iso);
  d.setDate(d.getDate() - ((d.getDay() + 6) % 7)); // back to Monday (Mon=0)
  return fmtISO(d);
}
// Step a reference date by ±1 unit of the given period. Month steps anchor to
// the 1st first, so e.g. Mar 31 − 1mo lands in February, not "Mar 3".
export function shiftPeriod(iso: string, period: string, delta: number): string {
  const d = isoToDate(iso);
  if (period === "Day") d.setDate(d.getDate() + delta);
  else if (period === "Week") d.setDate(d.getDate() + delta * 7);
  else { d.setDate(1); d.setMonth(d.getMonth() + delta); }
  return fmtISO(d);
}
// Is `iso` inside the *current* (today's) day/week/month for this period?
export function isCurrentPeriod(iso: string, period: string): boolean {
  const t = todayISO();
  if (period === "Day") return iso === t;
  if (period === "Month") return iso.slice(0, 7) === t.slice(0, 7);
  return weekStartISO(iso) === weekStartISO(t);
}

// ── formatting helpers ──────────────────────────────────────────
export const fmtTokens = (m: number) => {
  if (m >= 1) return m.toFixed(2) + "M";
  const k = m * 1000;
  // one decimal for sub-1K totals (e.g. "0.4K"), but only when it rounds to a
  // non-zero label — avoid a misleadingly precise "0.0K" for tiny values.
  if (k >= 0.05 && k < 1) return k.toFixed(1) + "K";
  return Math.round(k) + "K";
};
export const fmtInt = (n: number) => n.toLocaleString("en-US");
export const pct = (part: number, whole: number) => (whole > 0 ? Math.round((part / whole) * 100) : 0);
export function fmtMoney(v: number) {
  if (v >= 100000) return "$" + Math.round(v / 1000) + "K";
  if (v >= 10000) return "$" + (v / 1000).toFixed(1) + "K";
  return "$" + v.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
}

export function linePath(values: number[], w: number, h: number, pad = 2) {
  const n = values.length;
  // Self-protect against degenerate inputs: callers pass a fixed-length series
  // today, but a 0-point array threw (pts[0]) and a 1-point array gave NaN (÷0).
  if (n === 0) return { d: "", px: (_i: number) => pad, py: (_v: number) => h / 2, pts: [] as [number, number][] };
  const max = Math.max(...values), min = Math.min(...values);
  const range = max - min || 1;
  const px = (i: number) => (n === 1 ? w / 2 : pad + (i / (n - 1)) * (w - pad * 2));
  const py = (v: number) => pad + (1 - (v - min) / range) * (h - pad * 2);
  const pts = values.map((v, i) => [px(i), py(v)] as [number, number]);
  let d = `M ${pts[0][0].toFixed(1)} ${pts[0][1].toFixed(1)}`;
  for (let i = 0; i < pts.length - 1; i++) {
    const p0 = pts[i - 1] || pts[i], p1 = pts[i], p2 = pts[i + 1], p3 = pts[i + 2] || p2;
    const c1x = p1[0] + (p2[0] - p0[0]) / 6, c1y = p1[1] + (p2[1] - p0[1]) / 6;
    const c2x = p2[0] - (p3[0] - p1[0]) / 6, c2y = p2[1] - (p3[1] - p1[1]) / 6;
    d += ` C ${c1x.toFixed(1)} ${c1y.toFixed(1)}, ${c2x.toFixed(1)} ${c2y.toFixed(1)}, ${p2[0].toFixed(1)} ${p2[1].toFixed(1)}`;
  }
  return { d, px, py, pts };
}

// ── theme ────────────────────────────────────────────────────────
export interface Theme {
  ui: string; mono: string; display: string;
  accent: string; accentSoft: string; cacheCol: string;
  text: string; dim: string; faint: string;
  gridLine: string; card: string;
  segBg: string; segBorder: string; segOnBg: string; segOnText: string; segOffText: string; segOnShadow: string;
  tip: string;
}
export const TH: Record<"dark" | "light", Theme> = {
  dark: {
    ui: "'IBM Plex Sans', system-ui, sans-serif",
    mono: "'IBM Plex Mono', ui-monospace, monospace",
    display: "'Space Grotesk', system-ui, sans-serif",
    accent: "#27b06e", accentSoft: "#5fcf9c", cacheCol: "#5a6660",
    text: "rgba(255,255,255,0.94)", dim: "rgba(255,255,255,0.52)", faint: "rgba(255,255,255,0.32)",
    gridLine: "rgba(255,255,255,0.06)", card: "#1f2226",
    segBg: "rgba(255,255,255,0.06)", segBorder: "rgba(255,255,255,0.09)",
    segOnBg: "rgba(255,255,255,0.15)", segOnText: "#fff", segOffText: "rgba(255,255,255,0.55)",
    segOnShadow: "0 1px 2px rgba(0,0,0,0.35)", tip: "#34383d",
  },
  light: {
    ui: "'IBM Plex Sans', system-ui, sans-serif",
    mono: "'IBM Plex Mono', ui-monospace, monospace",
    display: "'Space Grotesk', system-ui, sans-serif",
    accent: "#178a55", accentSoft: "#8fd9b4", cacheCol: "#aeb8b2",
    text: "rgba(17,22,19,0.94)", dim: "rgba(17,22,19,0.5)", faint: "rgba(17,22,19,0.32)",
    gridLine: "rgba(0,0,0,0.06)", card: "#ffffff",
    segBg: "rgba(0,0,0,0.05)", segBorder: "rgba(0,0,0,0.07)",
    segOnBg: "#ffffff", segOnText: "#111", segOffText: "rgba(0,0,0,0.5)",
    segOnShadow: "0 1px 2px rgba(0,0,0,0.12)", tip: "#1d2420",
  },
};

// ── color presets ──────────────────────────────────────────────────
// Curated palettes the user can switch between (independent of Dark/Light/
// System). A preset only swaps the accent family + the model-chart ramp; the
// neutral chrome (bg/text/grid) still comes from the Dark/Light base above, so
// every preset stays legible in both modes. `ramp` recolors the model bars /
// cost donut (rank order, vivid→pale + one muted tail), matching the backend's
// 5-slot + overflow scheme.
export type PresetId = "emerald" | "azure" | "violet" | "amber" | "graphite";
interface PresetVars { accent: string; accentSoft: string; ramp: string[] }
export interface Preset { id: PresetId; name: string; dark: PresetVars; light: PresetVars }
export const PRESET_OVERFLOW = "#79817b";
export const PRESETS: Preset[] = [
  { id: "emerald", name: "Emerald",
    dark:  { accent: "#27b06e", accentSoft: "#5fcf9c", ramp: ["#1f9d63", "#34c27e", "#6ad0a0", "#a7e3c5", "#4b5a52"] },
    light: { accent: "#178a55", accentSoft: "#8fd9b4", ramp: ["#178a55", "#2fa86e", "#66c398", "#a7e3c5", "#8fa39a"] } },
  { id: "azure", name: "Azure",
    dark:  { accent: "#3f9bf5", accentSoft: "#84c1fb", ramp: ["#2f8fef", "#54a4f5", "#84c1fb", "#bcdcfd", "#4b5563"] },
    light: { accent: "#1f7fe0", accentSoft: "#8fc0f5", ramp: ["#1f7fe0", "#4a97ea", "#82bcf3", "#bcdcfd", "#94a3b8"] } },
  { id: "violet", name: "Violet",
    dark:  { accent: "#a279ef", accentSoft: "#c7adf7", ramp: ["#9061e8", "#a87df0", "#c7adf7", "#e2d2fb", "#524b63"] },
    light: { accent: "#7c4ddb", accentSoft: "#c4b5fd", ramp: ["#7c4ddb", "#976fe6", "#bda2f2", "#ddccfb", "#9a94a8"] } },
  { id: "amber", name: "Amber",
    dark:  { accent: "#f0a53f", accentSoft: "#f7cd8f", ramp: ["#ec9224", "#f2ab52", "#f7cd8f", "#fbe6c1", "#5a5347"] },
    light: { accent: "#d9821a", accentSoft: "#f2c88a", ramp: ["#d9821a", "#e6a047", "#f0c286", "#f8dcb0", "#a89a87"] } },
  { id: "graphite", name: "Graphite",
    dark:  { accent: "#9aa4a0", accentSoft: "#c3ccc8", ramp: ["#8b938f", "#a4aca8", "#bcc3bf", "#d6dbd8", "#4b5a52"] },
    light: { accent: "#5f6b66", accentSoft: "#a7b0ab", ramp: ["#5f6b66", "#7c8681", "#9aa39e", "#c0c7c3", "#8fa39a"] } },
];

const presetVars = (dark: boolean, preset: PresetId): PresetVars =>
  (PRESETS.find((x) => x.id === preset) ?? PRESETS[0])[dark ? "dark" : "light"];

/// The Dark/Light base theme with the chosen preset's accent family applied.
export function themeFor(dark: boolean, preset: PresetId): Theme {
  const p = presetVars(dark, preset);
  return { ...TH[dark ? "dark" : "light"], accent: p.accent, accentSoft: p.accentSoft };
}
/// The preset's model-chart color ramp (5 rank slots; overflow uses PRESET_OVERFLOW).
export function rampFor(dark: boolean, preset: PresetId): string[] {
  return presetVars(dark, preset).ramp;
}

// The busiest contiguous 3-hour block of a 24-bucket hour histogram, plus the
// share of the period's tokens it holds. null when there's no usage.
export function peakHours(hourly: number[]): { start: number; end: number; share: number } | null {
  if (!hourly || hourly.length !== 24) return null;
  const total = hourly.reduce((s, v) => s + v, 0);
  if (total <= 0) return null;
  const win = 3;
  let best = 0, bestSum = -1;
  for (let i = 0; i <= 24 - win; i++) {
    let s = 0;
    for (let j = 0; j < win; j++) s += hourly[i + j];
    if (s > bestSum) { bestSum = s; best = i; }
  }
  return { start: best, end: best + win, share: bestSum / total };
}
// Render an hour boundary pair as a friendly 12-hour range, e.g. "9am–12pm",
// "9–11am" (shared meridiem collapses), "10pm–1am".
export function fmtHourRange(start: number, end: number): string {
  const parts = (h: number) => {
    const hh = ((h % 24) + 24) % 24;
    return { d: hh % 12 === 0 ? 12 : hh % 12, ap: hh < 12 ? "am" : "pm" };
  };
  const a = parts(start), b = parts(end);
  return a.ap === b.ap ? `${a.d}–${b.d}${b.ap}` : `${a.d}${a.ap}–${b.d}${b.ap}`;
}

// Straight-line projection of a current, partially-elapsed week/month to its
// full-period tokens+cost, from the fraction of days elapsed (today counts as a
// whole day — a mild over-count that keeps the pace from lagging). null for Day,
// past periods, or when nothing's been used yet.
export function projection(period: string, tokens: number, cost: number, isCurrent: boolean):
  { tokens: number; cost: number; label: string } | null {
  if (!isCurrent || tokens <= 0) return null;
  const now = new Date();
  let frac: number, label: string;
  if (period === "Week") {
    frac = (((now.getDay() + 6) % 7) + 1) / 7; // Mon=1 … Sun=7
    label = "this week";
  } else if (period === "Month") {
    const dim = new Date(now.getFullYear(), now.getMonth() + 1, 0).getDate();
    frac = now.getDate() / dim;
    label = "this month";
  } else {
    return null; // Day: nothing to project
  }
  if (frac >= 1) return null; // period complete → the actual IS the total
  return { tokens: tokens / frac, cost: cost / frac, label };
}

// Day-of-week token rhythm from the heatmap window (~26 weeks): 7 buckets
// (Mon→Sun, M tokens), the busiest weekday index, and the weekend share (%).
// null when the heatmap is empty / all-zero.
export function weekdayRhythm(heatmap: { date: string; tokens: number }[]):
  { bars: number[]; busiest: number; weekendPct: number } | null {
  if (!heatmap || !heatmap.length) return null;
  const bars = [0, 0, 0, 0, 0, 0, 0]; // Mon…Sun
  for (const d of heatmap) {
    const wd = (new Date(d.date + "T00:00:00").getDay() + 6) % 7; // Sun=0 → Mon=0
    bars[wd] += d.tokens;
  }
  const total = bars.reduce((s, v) => s + v, 0);
  if (total <= 0) return null;
  let busiest = 0;
  for (let i = 1; i < 7; i++) if (bars[i] > bars[busiest]) busiest = i;
  return { bars, busiest, weekendPct: ((bars[5] + bars[6]) / total) * 100 };
}

// Current run of consecutive active days ending today (heatmap is oldest→newest,
// last entry = today). A not-yet-started today (0 tokens) doesn't break the run.
export function activeStreak(heatmap: { tokens: number }[]): number {
  let i = heatmap.length - 1;
  if (i >= 0 && heatmap[i].tokens <= 0) i--; // today may not have started yet
  let s = 0;
  for (; i >= 0 && heatmap[i].tokens > 0; i--) s++;
  return s;
}

export function fmtHeatDate(iso: string) {
  const d = new Date(iso + "T00:00:00");
  return d.toLocaleDateString("en-US", { year: "numeric", month: "short", day: "numeric" });
}
