import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { invoke } from "@tauri-apps/api/core";
import { domToPng } from "modern-screenshot";
import {
  Dashboard, Workspace, PeriodReport, HeatDay, ModelStat, Theme,
  PresetId, PRESETS, PRESET_OVERFLOW, themeFor, rampFor,
  fetchWorkspace, fetchPeriod, todayISO, shiftPeriod, isCurrentPeriod,
  fmtInt, fmtTokens, fmtMoney, pct, peakHours, fmtHourRange,
} from "./data";
import {
  TokenGlyph, Segmented, BarChart, Sparkline, CostDonut, BarList, TokenBarList, Heatmap, TrendChart,
} from "./charts";

// Count up to `target`. Restarts from 0 whenever `resetKey` changes (popover
// open / period switch); on a live value change it eases from the current
// value to the new one instead of snapping back to 0.
function useCountUp(target: number, resetKey: string, active: boolean, duration = 850): number {
  const [val, setVal] = useState(0);
  const valRef = useRef(0);
  const keyRef = useRef<string | null>(null);
  const rafRef = useRef(0);
  // useLayoutEffect so the reset-to-0 is committed *before* the browser paints
  // (otherwise the old/final value flashes for a frame before counting up).
  useLayoutEffect(() => {
    cancelAnimationFrame(rafRef.current);
    const set = (v: number) => { valRef.current = v; setVal(v); };
    // while the popover is hidden, hold at 0 so the next open starts clean
    if (!active) { keyRef.current = null; set(0); return; }
    const reset = keyRef.current !== resetKey;
    keyRef.current = resetKey;
    // open / period switch → start from 0 (paint it now); live update → ease
    // from the current value to the new one.
    let from = valRef.current;
    if (reset) { from = 0; set(0); }
    const start = performance.now();
    const ease = (t: number) => 1 - Math.pow(1 - t, 3); // easeOutCubic
    const tick = (now: number) => {
      const p = Math.min(1, (now - start) / duration);
      set(from + (target - from) * ease(p));
      if (p < 1) rafRef.current = requestAnimationFrame(tick);
    };
    rafRef.current = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(rafRef.current);
  }, [resetKey, target, active, duration]);
  return val;
}

function Delta({ v, theme }: { v: number; theme: Theme }) {
  const up = v >= 0;
  // Usage/cost going up is "bad" → red; going down is "good" → green.
  const col = up ? "#e0795f" : theme.accent;
  return (
    <span style={{ font: `600 10px ${theme.mono}`, color: col, display: "inline-flex", alignItems: "center", gap: 2,
      padding: "1.5px 5px", borderRadius: 5, background: up ? "rgba(224,121,95,0.16)" : "rgba(39,176,110,0.14)" }}>
      {up ? "▲" : "▼"}{Math.abs(Math.round(v))}%
    </span>
  );
}

// Round each value's share to 1 decimal (%) via largest-remainder apportionment,
// so the displayed percentages sum to exactly 100.0% (plain rounding wouldn't).
function sharePcts(values: number[]): number[] {
  const total = values.reduce((s, v) => s + v, 0);
  if (total <= 0) return values.map(() => 0);
  const UNITS = 1000; // work in 0.1% units; target is 100.0%
  const raw = values.map((v) => (v / total) * UNITS);
  const units = raw.map(Math.floor);
  const left = Math.round(UNITS - units.reduce((s, f) => s + f, 0));
  raw
    .map((r, i) => ({ i, frac: r - Math.floor(r) }))
    .sort((a, b) => b.frac - a.frac)
    .slice(0, left)
    .forEach(({ i }) => (units[i] += 1));
  return units.map((u) => u / 10);
}

function ModelRow({ m, max, theme, share }: { m: ModelStat; max: number; theme: Theme; share: number }) {
  // 1-decimal share; whole numbers drop the ".0" (100% not 100.0%).
  const pctStr = share % 1 === 0 ? share.toFixed(0) : share.toFixed(1);
  return (
    <div style={{ display: "flex", alignItems: "center", gap: 9, padding: "5px 0" }}>
      <span style={{ width: 7, height: 7, borderRadius: 2, background: m.color, flex: "0 0 auto" }} />
      <div style={{ minWidth: 0, flex: "0 0 118px" }}>
        <div style={{ font: `500 11.5px ${theme.ui}`, color: theme.text, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>{m.name}</div>
      </div>
      <div style={{ flex: 1, height: 5, borderRadius: 3, background: theme.gridLine, overflow: "hidden" }}>
        <div style={{ width: `${(m.tokens / max) * 100}%`, height: "100%", background: m.color, borderRadius: 3 }} />
      </div>
      <span style={{ font: `500 10.5px ${theme.mono}`, color: theme.dim, flex: "0 0 auto", width: 42, textAlign: "right" }}>{fmtTokens(m.tokens)}</span>
      <span style={{ font: `600 10.5px ${theme.mono}`, color: theme.text, flex: "0 0 auto", width: 40, textAlign: "right" }}>{pctStr}%</span>
    </div>
  );
}

function MiniStat({ label, value, sub, theme, accent, children }:
  { label: string; value: string; sub?: string; theme: Theme; accent?: string; children?: React.ReactNode }) {
  return (
    <div style={{ background: theme.gridLine, borderRadius: 9, padding: "9px 10px", minWidth: 0 }}>
      <div style={{ font: `500 9.5px ${theme.ui}`, color: theme.dim, letterSpacing: ".04em", textTransform: "uppercase" }}>{label}</div>
      <div style={{ display: "flex", alignItems: "flex-end", justifyContent: "space-between", marginTop: 3, gap: 6 }}>
        <span style={{ font: `600 17px/1 ${theme.mono}`, color: accent || theme.text }}>{value}</span>
        {children}
      </div>
      {sub && <div style={{ font: `500 9px ${theme.mono}`, color: theme.faint, marginTop: 3 }}>{sub}</div>}
    </div>
  );
}

// Input/Output legend: full words by default, abbreviated to In/Out only
// when the row would otherwise overflow the available width.
function SplitLegend({ t, inputM, outputM, cachedPct }:
  { t: Theme; inputM: number; outputM: number; cachedPct: number }) {
  const ref = useRef<HTMLDivElement>(null);
  const [compact, setCompact] = useState(false);
  const key = `${inputM}|${outputM}|${cachedPct}`;
  // reset to full labels whenever the numbers change, then re-measure
  useLayoutEffect(() => { setCompact(false); }, [key]);
  useLayoutEffect(() => {
    const el = ref.current;
    if (el && !compact && el.scrollWidth > el.clientWidth + 1) setCompact(true);
  });
  return (
    <div ref={ref} style={{
      display: "flex", alignItems: "center", gap: 14,
      font: `500 10px ${t.mono}`, color: t.dim, marginBottom: 14, whiteSpace: "nowrap", overflow: "hidden",
    }}>
      <span><span style={{ color: t.accent }}>●</span> {compact ? "In" : "Input"} {inputM.toFixed(2)}M</span>
      <span><span style={{ color: t.accentSoft }}>●</span> {compact ? "Out" : "Output"} {outputM.toFixed(2)}M</span>
      <span style={{ color: t.faint }}>{cachedPct}% cached</span>
    </div>
  );
}

const SectionRule = ({ t, m = "12px 0 10px" }: { t: Theme; m?: string }) => (
  <div style={{ height: 1, background: t.gridLine, margin: m }} />
);
const Label = ({ t, children }: { t: Theme; children: React.ReactNode }) => (
  <span style={{ font: `600 10px ${t.ui}`, color: t.dim, letterSpacing: ".05em", textTransform: "uppercase", whiteSpace: "nowrap" }}>{children}</span>
);

function ThemeToggle({ pref, theme, onCycle }: { pref: "dark" | "light" | "system"; theme: Theme; onCycle: () => void }) {
  const t = theme;
  // Single button cycling Dark → Light → System; the icon shows the current mode.
  const label = pref === "system" ? "System" : pref === "dark" ? "Dark" : "Light";
  return (
    <button onClick={onCycle} title={`Theme: ${label} (click to change)`} aria-label={`theme: ${label}`} style={{
      display: "inline-flex", alignItems: "center", justifyContent: "center",
      width: 26, height: 26, borderRadius: 7, cursor: "pointer", padding: 0,
      background: t.segBg, border: `1px solid ${t.segBorder}`, color: t.dim,
    }}>
      {pref === "light" ? (
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke={t.dim} strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
          <circle cx="12" cy="12" r="4.2" />
          <path d="M12 2.5v2.2M12 19.3v2.2M2.5 12h2.2M19.3 12h2.2M5.1 5.1l1.6 1.6M17.3 17.3l1.6 1.6M18.9 5.1l-1.6 1.6M6.7 17.3l-1.6 1.6" />
        </svg>
      ) : pref === "dark" ? (
        <svg width="14" height="14" viewBox="0 0 24 24" fill={t.dim} stroke="none">
          <path d="M21 12.9A9 9 0 1 1 11.1 3a7.2 7.2 0 0 0 9.9 9.9z" />
        </svg>
      ) : (
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke={t.dim} strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
          <rect x="3" y="4.5" width="18" height="12.5" rx="1.6" />
          <path d="M8.5 20.5h7M12 17v3.5" />
        </svg>
      )}
    </button>
  );
}

// Color-preset picker: a swatch button that opens a small row of preset
// swatches (matches the "preset themes only" scope — pick a palette, no
// per-color editing). Closes on select or an outside click.
function ThemePicker({ t, dark, preset, onPick }: { t: Theme; dark: boolean; preset: PresetId; onPick: (id: PresetId) => void }) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => { if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false); };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [open]);
  return (
    <div ref={ref} style={{ position: "relative", display: "inline-flex" }}>
      <button onClick={() => setOpen((o) => !o)} title="Theme color" aria-label="theme color" style={{
        display: "inline-flex", alignItems: "center", justifyContent: "center",
        width: 26, height: 26, borderRadius: 7, cursor: "pointer", padding: 0,
        background: t.segBg, border: `1px solid ${t.segBorder}`,
      }}>
        <span style={{ width: 13, height: 13, borderRadius: "50%", background: t.accent, boxShadow: "inset 0 0 0 1px rgba(0,0,0,0.18)" }} />
      </button>
      {open && (
        <div style={{
          position: "absolute", top: 32, right: 0, zIndex: 30,
          display: "flex", gap: 6, padding: 8, borderRadius: 10,
          background: t.card, border: `1px solid ${t.segBorder}`, boxShadow: "0 10px 30px rgba(0,0,0,0.35)",
        }}>
          {PRESETS.map((p) => {
            const c = p[dark ? "dark" : "light"].accent;
            const on = p.id === preset;
            return (
              <button key={p.id} onClick={() => { onPick(p.id); setOpen(false); }} title={p.name} aria-label={p.name} style={{
                width: 22, height: 22, borderRadius: "50%", cursor: "pointer", padding: 0, background: c,
                border: on ? `2px solid ${t.text}` : "2px solid transparent",
                boxShadow: on ? "none" : "inset 0 0 0 1px rgba(0,0,0,0.12)",
              }} />
            );
          })}
        </div>
      )}
    </div>
  );
}

function ScreenshotButton({ theme, busy, onClick }: { theme: Theme; busy: boolean; onClick: () => void }) {
  const t = theme;
  return (
    <button onClick={onClick} disabled={busy} title="Save screenshot to Desktop" aria-label="save screenshot" style={{
      display: "inline-flex", alignItems: "center", justifyContent: "center",
      width: 26, height: 26, borderRadius: 7, cursor: busy ? "default" : "pointer", padding: 0,
      background: t.segBg, border: `1px solid ${t.segBorder}`, color: t.dim,
    }}>
      {busy ? (
        <svg className="om-spin" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke={t.dim} strokeWidth="2.6" strokeLinecap="round">
          <path d="M12 3a9 9 0 1 0 9 9" />
        </svg>
      ) : (
        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke={t.dim} strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round">
          <path d="M3 8.5A2.5 2.5 0 0 1 5.5 6h1.7l1.1-1.6A1.5 1.5 0 0 1 9.5 4h5a1.5 1.5 0 0 1 1.2.4L16.8 6h1.7A2.5 2.5 0 0 1 21 8.5v8A2.5 2.5 0 0 1 18.5 19h-13A2.5 2.5 0 0 1 3 16.5z" />
          <circle cx="12" cy="12.2" r="3.4" />
        </svg>
      )}
    </button>
  );
}

// Account switcher: one pill per Claude account plus an "All" aggregate.
// Only rendered when more than one account exists (single-account users see
// no tab bar at all — identical to the pre-multi-account layout).
// The per-account labels default to the name derived from each account's
// .claude.json, but are user-editable: double-click a tab to rename it (Enter
// or blur saves; Escape cancels; clearing it resets to the auto-derived name).
// The "All" tab is fixed and not renamable.
function AccountTabs({ t, tabs, activeTab, onSelect, onRename }:
  { t: Theme; tabs: { id: string; label: string }[]; activeTab: string; onSelect: (id: string) => void; onRename: (id: string, label: string) => void }) {
  const [editingId, setEditingId] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);

  const startEdit = (id: string, label: string) => { setDraft(label); setEditingId(id); };
  const commit = () => {
    if (editingId !== null) onRename(editingId, draft);
    setEditingId(null);
  };
  // Focus + select the field as soon as it appears so the name is ready to type over.
  useEffect(() => {
    if (editingId !== null) { inputRef.current?.focus(); inputRef.current?.select(); }
  }, [editingId]);

  return (
    <div data-no-drag="" style={{
      display: "flex", justifyContent: "center", flexWrap: "wrap", gap: 4,
      padding: "11px 12px 3px", cursor: "default",
    }}>
      {tabs.map((tab) => {
        const on = tab.id === activeTab;
        const editable = tab.id !== "all";
        if (editingId === tab.id) {
          return (
            <input key={tab.id} ref={inputRef} value={draft}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") commit();
                else if (e.key === "Escape") setEditingId(null);
              }}
              onBlur={commit}
              style={{
                font: `600 11px ${t.ui}`, padding: "4px 10px", borderRadius: 8, width: 100,
                border: `1px solid ${t.accent}`, background: t.card, color: t.text, outline: "none",
              }} />
          );
        }
        return (
          <button key={tab.id} onClick={() => onSelect(tab.id)}
            onDoubleClick={editable ? () => startEdit(tab.id, tab.label) : undefined}
            title={editable ? `${tab.label} — double-click to rename` : tab.label}
            style={{
              font: `600 11px ${t.ui}`, padding: "4px 11px", borderRadius: 8, cursor: "pointer",
              maxWidth: 130, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis",
              border: `1px solid ${on ? "transparent" : t.segBorder}`,
              background: on ? t.accent : t.segBg,
              color: on ? "#fff" : t.segOffText,
              transition: "background .15s, color .15s",
            }}>{tab.label}</button>
        );
      })}
    </div>
  );
}

function Panel({ report, heatmap, period, onPeriod, dark, themePref, onToggleTheme, openGen, active, tabs, activeTab, onSelectTab, onRename, preset, onPickPreset, isCurrent, onPrev, onNext, onToday, onDrillDay, onTrendPick, loading }: { report: PeriodReport; heatmap: HeatDay[]; period: "Day" | "Week" | "Month"; onPeriod: (p: string) => void; dark: boolean; themePref: "dark" | "light" | "system"; onToggleTheme: () => void; openGen: number; active: boolean; tabs: { id: string; label: string }[]; activeTab: string; onSelectTab: (id: string) => void; onRename: (id: string, label: string) => void; preset: PresetId; onPickPreset: (id: PresetId) => void; isCurrent: boolean; onPrev: () => void; onNext: () => void; onToday: () => void; onDrillDay: (iso: string) => void; onTrendPick: (iso: string) => void; loading: boolean }) {
  const t = themeFor(dark, preset);
  const ramp = rampFor(dark, preset);
  // Drag the popover by its body (Windows/Linux only — macOS uses the menu-bar
  // NSPanel and is gated out). A real OS window-drag begins only once the
  // pointer moves past a small threshold, so a plain click still clicks through
  // / dismisses and never arms the hide-suppression guard.
  const canDrag = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window && !navigator.userAgent.includes("Macintosh");
  const dragRef = useRef<{ x: number; y: number } | null>(null);
  const P = report;
  const M = P.metrics;
  // animated Total tokens: counts up from 0 on each open / period / date switch;
  // held at 0 while the popover is hidden so it never flashes the final value.
  const animTotal = useCountUp(M.totalTokens, `${activeTab}:${period}:${P.range}:${openGen}`, active);
  // Recolor model slices with the active preset's ramp (rank order), so the
  // model bars + cost donut match the chosen theme instead of the fixed green.
  const models = P.models.map((m, i) => ({ ...m, color: i < ramp.length ? ramp[i] : PRESET_OVERFLOW }));
  // Hide noise: 0% token-share rows, and $0 entries in the cost donut.
  // Show models whose share is at least 0.1% when rounded to 1 decimal; below
  // that it'd render a meaningless "0.0%" (a negligible token share). Such a
  // model can still appear under Cost if it has a non-zero cost.
  const tokenModels = models.filter(
    (m) => Math.round((m.tokens / (M.totalTokens || 1)) * 1000) / 10 >= 0.1
  );
  const costModels = models.filter((m) => m.cost > 0);
  // models that were used but have no LiteLLM pricing (cost unknown, not $0)
  const unpricedModels = models.filter((m) => !m.priced && m.tokens > 0);
  const maxM = Math.max(...tokenModels.map((m) => m.tokens), 1e-9);
  // Per-row shares that sum to exactly 100.0% (largest-remainder over visible rows).
  const tokenShares = sharePcts(tokenModels.map((m) => m.tokens));
  const trendSub = { Day: "today 24h", Week: "this week", Month: "this month" }[period];
  const trendLabel = { Day: "Last 14 days", Week: "Last 12 weeks", Month: "Last 6 months" }[period];
  const navLabel = period === "Day" && isCurrent ? "Today" : P.range;
  const arrowStyle = (disabled: boolean): React.CSSProperties => ({
    display: "inline-flex", alignItems: "center", justifyContent: "center",
    width: 24, height: 24, borderRadius: 6, padding: 0, font: `600 15px ${t.ui}`, lineHeight: 1,
    background: t.segBg, border: `1px solid ${t.segBorder}`,
    color: disabled ? t.faint : t.dim, cursor: disabled ? "default" : "pointer", opacity: disabled ? 0.5 : 1,
  });

  // screenshot capture: rasterize the full panel card to a PNG and hand it to
  // the Rust `save_screenshot` command (browser preview falls back to a download).
  const [shotBusy, setShotBusy] = useState(false);
  const [toast, setToast] = useState<{ msg: string; ok: boolean } | null>(null);
  const toastTimer = useRef<number | null>(null);
  const showToast = (msg: string, ok: boolean) => {
    if (toastTimer.current) window.clearTimeout(toastTimer.current);
    setToast({ msg, ok });
    toastTimer.current = window.setTimeout(() => setToast(null), 1800);
  };
  const captureScreenshot = async () => {
    if (shotBusy) return;
    const el = document.querySelector<HTMLElement>(".om-scroll");
    if (!el) { showToast("Nothing to capture", false); return; }
    setShotBusy(true);
    try {
      // explicit width/height = full scrollable content, not just the viewport;
      // filter drops the capture button itself (and its in-flight spinner) so
      // the saved image is a clean dashboard, not a shot of the button.
      const dataUrl = await domToPng(el, {
        scale: 2,
        backgroundColor: dark ? "#1f2226" : "#ffffff",
        width: el.scrollWidth,
        height: el.scrollHeight,
        filter: (n) => !(n instanceof HTMLElement && n.getAttribute("aria-label") === "save screenshot"),
      });
      const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
      if (inTauri) {
        await invoke<string>("save_screenshot", { dataUrl });
        showToast("Saved to Desktop", true);
      } else {
        const a = document.createElement("a");
        a.href = dataUrl;
        a.download = "tokenscope.png";
        document.body.appendChild(a);
        a.click();
        a.remove();
        showToast("Downloaded", true);
      }
    } catch {
      showToast("Screenshot failed", false);
    } finally {
      setShotBusy(false);
    }
  };

  return (
    <div style={{
      width: "100%", height: "100vh", overflow: "hidden", boxSizing: "border-box",
      position: "relative",
      background: "transparent", padding: 0,
      fontFamily: t.ui,
    }}>
      <div className="om-scroll"
        onMouseDown={canDrag ? (e) => {
          // Record the press; the real drag only starts once the pointer moves
          // past the threshold (onMouseMove). Skip interactive controls
          // (data-no-drag) and non-left buttons so clicks still register.
          if (e.button !== 0) return;
          if ((e.target as HTMLElement).closest("[data-no-drag]")) return;
          dragRef.current = { x: e.clientX, y: e.clientY };
        } : undefined}
        onMouseMove={canDrag ? (e) => {
          const s = dragRef.current;
          if (!s) return;
          const dx = e.clientX - s.x, dy = e.clientY - s.y;
          if (dx * dx + dy * dy >= 16) { // ~4px → a drag, not a click
            dragRef.current = null;
            invoke("begin_drag").catch(() => {});
          }
        } : undefined}
        onMouseUp={canDrag ? () => { dragRef.current = null; } : undefined}
        style={{
        width: "100%", height: "100%", overflowY: "auto",
        borderRadius: 12, background: dark ? "#1f2226" : "#ffffff",
        border: `1px solid ${dark ? "rgba(255,255,255,0.10)" : "rgba(0,0,0,0.08)"}`,
        padding: 0, color: t.text, cursor: canDrag ? "grab" : undefined,
      }}>
        {/* sticky header — stays put while the body scrolls */}
        <div style={{
          position: "sticky", top: 0, zIndex: 10,
          background: dark ? "#1f2226" : "#ffffff",
          borderBottom: `1px solid ${t.gridLine}`,
        }}>
          {/* account switcher (only when more than one account) */}
          {tabs.length > 1 && <AccountTabs t={t} tabs={tabs} activeTab={activeTab} onSelect={onSelectTab} onRename={onRename} />}
          <div style={{
            display: "flex", alignItems: "center", justifyContent: "space-between",
            padding: tabs.length > 1 ? "6px 15px 12px" : "15px 15px 12px",
          }}>
            <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
              <TokenGlyph color={t.accent} size={16} />
              <div data-no-drag="" style={{ cursor: "default" }}>
                <Segmented value={period} theme={t} onSelect={onPeriod} />
              </div>
            </div>
            <div data-no-drag="" style={{ display: "flex", alignItems: "center", gap: 8, cursor: "default" }}>
              <ThemeToggle pref={themePref} theme={t} onCycle={onToggleTheme} />
              <ThemePicker t={t} dark={dark} preset={preset} onPick={onPickPreset} />
              <ScreenshotButton theme={t} busy={shotBusy} onClick={captureScreenshot} />
            </div>
          </div>
        </div>
        {/* scrolling body */}
        <div style={{ padding: "14px 15px 15px" }}>
        {/* period navigation: ‹ range › with a Today reset when viewing the past */}
        <div data-no-drag="" style={{ display: "flex", alignItems: "center", justifyContent: "space-between", marginBottom: 12, cursor: "default" }}>
          <button onClick={onPrev} aria-label="previous period" title="Previous" style={arrowStyle(false)}>‹</button>
          <div style={{ display: "flex", alignItems: "center", gap: 8, minWidth: 0 }}>
            <span style={{ font: `600 12px ${t.ui}`, color: t.text, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>{navLabel}</span>
            {loading && <span style={{ font: `500 10px ${t.mono}`, color: t.faint }}>…</span>}
            {!isCurrent && <button onClick={onToday} style={{ font: `600 9.5px ${t.ui}`, color: t.accent, background: "none", border: "none", cursor: "pointer", padding: 0 }}>Today</button>}
          </div>
          <button onClick={onNext} disabled={isCurrent} aria-label="next period" title="Next" style={arrowStyle(isCurrent)}>›</button>
        </div>
        {/* hero */}
        <div style={{ display: "flex", alignItems: "flex-end", justifyContent: "space-between", marginBottom: 10 }}>
          <div>
            <div style={{ font: `500 10px ${t.ui}`, color: t.dim, letterSpacing: ".04em", textTransform: "uppercase" }}>Total tokens</div>
            <div style={{ display: "flex", alignItems: "baseline", gap: 8, marginTop: 3 }}>
              <span style={{ font: `600 30px ${t.mono}`, color: t.text, letterSpacing: "-.01em" }}>{animTotal.toFixed(2)}<span style={{ font: `500 15px ${t.mono}`, color: t.dim, marginLeft: 2 }}>M</span></span>
              {Math.round(M.deltaTokens) !== 0 && <Delta v={M.deltaTokens} theme={t} />}
            </div>
          </div>
          <div style={{ textAlign: "right" }}>
            <div style={{ font: `500 10px ${t.ui}`, color: t.dim }}>Est. cost</div>
            <div style={{ font: `600 18px ${t.mono}`, color: t.accent, marginTop: 2 }}>${M.cost.toFixed(2)}</div>
          </div>
        </div>
        {/* input(+cache) / output split — 2-colour; cache hits fold into input.
            When there's no usage the bar is just the empty track (no slivers). */}
        <div style={{ display: "flex", gap: 0, height: 7, borderRadius: 4, overflow: "hidden", marginBottom: 5, background: t.gridLine }}>
          {M.totalTokens > 0 && <>
            <div style={{ flexGrow: Math.max(M.inputTokens + M.cacheTokens, 1e-6), flexBasis: 0, minWidth: 4, background: t.accent }} />
            <div style={{ flexGrow: Math.max(M.outputTokens, 1e-6), flexBasis: 0, minWidth: 4, background: t.accentSoft }} />
          </>}
        </div>
        <SplitLegend t={t} inputM={M.inputTokens + M.cacheTokens} outputM={M.outputTokens} cachedPct={pct(M.cacheTokens, M.totalTokens)} />
        {/* cache-savings callout — what caching kept off the bill this period */}
        {M.cacheSavings > 0 && (
          <div style={{ display: "flex", alignItems: "center", gap: 5, marginTop: -9, marginBottom: 13, font: `600 10.5px ${t.mono}`, color: t.accent }}>
            <svg width="10" height="12" viewBox="0 0 10 12" fill={t.accent} style={{ flex: "0 0 auto" }} aria-hidden="true"><path d="M6 0 0 7h3l-1 5 6-7H5z" /></svg>
            Saved {fmtMoney(M.cacheSavings)} via cache
          </div>
        )}
        {/* bar chart — bars in Week/Month drill into that day */}
        <BarChart data={P.series} theme={t} height={84} onPick={(p) => onDrillDay(p.date)} />
        {(() => {
          // Busiest 3-hour window of the period — a quick "when do I work" read.
          const pk = peakHours(P.hourly);
          return pk ? (
            <div style={{ marginTop: 8, font: `500 9.5px ${t.mono}`, color: t.faint, display: "flex", alignItems: "center", gap: 5 }}>
              <svg width="11" height="11" viewBox="0 0 12 12" fill="none" stroke={t.faint} strokeWidth="1.3" style={{ flex: "0 0 auto" }} aria-hidden="true"><circle cx="6" cy="6" r="4.6" /><path d="M6 3.4V6l1.9 1.1" strokeLinecap="round" /></svg>
              Most active {fmtHourRange(pk.start, pk.end)} · {Math.round(pk.share * 100)}% of tokens
            </div>
          ) : null;
        })()}
        {/* zoomed-out trend line (click a point to jump to that period) */}
        <SectionRule t={t} m="14px 0 10px" />
        <div style={{ display: "flex", justifyContent: "space-between", alignItems: "baseline", marginBottom: 8 }}>
          <Label t={t}>Trend</Label>
          <span style={{ font: `500 9px ${t.mono}`, color: t.faint }}>{trendLabel}</span>
        </div>
        <TrendChart data={P.trend} theme={t} onPick={onTrendPick} />
        <SectionRule t={t} m="14px 0 10px" />
        {/* models */}
        <div style={{ marginBottom: 4 }}><Label t={t}>Tokens by model</Label></div>
        {tokenModels.length === 0 && <div style={{ font: `500 10.5px ${t.mono}`, color: t.faint, padding: "4px 0" }}>No usage in this period</div>}
        {tokenModels.map((m, i) => <ModelRow key={i} m={m} max={maxM} theme={t} share={tokenShares[i]} />)}
        <SectionRule t={t} m="10px 0 10px" />
        {/* cost donut */}
        <div style={{ marginBottom: 8 }}><Label t={t}>Cost by model</Label></div>
        {costModels.length > 0
          ? <CostDonut models={costModels} theme={t} size={costModels.length === 1 ? 84 : 100} thickness={costModels.length === 1 ? 13 : 15} palette={ramp} overflow={PRESET_OVERFLOW} />
          : <div style={{ font: `500 10.5px ${t.mono}`, color: t.faint }}>—</div>}
        {unpricedModels.length > 0 && (
          <div style={{ marginTop: 9, font: `500 9.5px/1.5 ${t.mono}`, color: t.faint }}>
            {unpricedModels.length} model{unpricedModels.length > 1 ? "s" : ""} without pricing data (cost not counted):{" "}
            <span style={{ color: t.dim }}>{unpricedModels.map((m) => m.name).join(", ")}</span>
          </div>
        )}
        {/* tokens by project — where the spend actually went (cwd basename) */}
        {P.projects.length > 0 && (
          <>
            <SectionRule t={t} m="12px 0 10px" />
            <div style={{ marginBottom: 6 }}><Label t={t}>Tokens by project</Label></div>
            <TokenBarList key={period} items={P.projects} theme={t} accent={t.accent} />
          </>
        )}
        <SectionRule t={t} m="12px 0 12px" />
        {/* footer stats */}
        <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 8 }}>
          <MiniStat label="Requests" value={fmtInt(M.requests)} sub={`${M.sessions} sessions`} theme={t}>
            <Sparkline values={P.reqTrend.length ? P.reqTrend : [0, 0]} theme={t} width={52} height={20} accent={t.accent} />
          </MiniStat>
          <MiniStat label="Cost trend" value={`$${M.cost.toFixed(2)}`} sub={trendSub} theme={t} accent={t.accent}>
            <Sparkline values={P.costTrend.length ? P.costTrend : [0, 0]} theme={t} width={52} height={20} accent={t.accent} />
          </MiniStat>
        </div>
        {/* tools — the real workhorses (Bash/Read/Edit…); header carries the
            subagent token share, since the Agent tool lives in this list too */}
        {P.tools.length > 0 && (
          <>
            <SectionRule t={t} />
            <div style={{ display: "flex", alignItems: "baseline", justifyContent: "space-between", marginBottom: 7 }}>
              <Label t={t}>Tools</Label>
              {M.subagentTokens > 0 && (
                <span style={{ font: `500 10px ${t.mono}`, color: t.faint, whiteSpace: "nowrap" }}>
                  <span style={{ color: t.text, fontWeight: 600 }}>{pct(M.subagentTokens, M.totalTokens)}%</span> via subagents
                </span>
              )}
            </div>
            <BarList key={period} items={P.tools} theme={t} accent={t.accent} />
          </>
        )}
        {/* MCP — shown whenever the user has installed MCP servers */}
        {M.servers > 0 && (
          <>
            <SectionRule t={t} />
            <div style={{ display: "flex", alignItems: "baseline", justifyContent: "space-between", marginBottom: 7 }}>
              <Label t={t}>MCP calls</Label>
              <span style={{ font: `500 10px ${t.mono}`, color: t.faint, whiteSpace: "nowrap" }}><span style={{ color: t.text, fontWeight: 600 }}>{fmtInt(M.mcpCalls)}</span> · {M.servers} servers</span>
            </div>
            {P.mcp.length > 0
              ? <BarList key={period} items={P.mcp} theme={t} accent={t.accent} />
              : <div style={{ font: `500 10px ${t.mono}`, color: t.faint, padding: "2px 0" }}>No MCP calls in this period</div>}
          </>
        )}
        {/* Skill — shown whenever the user has installed skills */}
        {M.skills > 0 && (
          <>
            <SectionRule t={t} />
            <div style={{ display: "flex", alignItems: "baseline", justifyContent: "space-between", marginBottom: 7 }}>
              <Label t={t}>Skill calls</Label>
              <span style={{ font: `500 10px ${t.mono}`, color: t.faint, whiteSpace: "nowrap" }}><span style={{ color: t.text, fontWeight: 600 }}>{fmtInt(M.skillCalls)}</span> · {M.skills} skills</span>
            </div>
            {P.skills.length > 0
              ? <BarList key={period} items={P.skills} theme={t} accent={t.accent} />
              : <div style={{ font: `500 10px ${t.mono}`, color: t.faint, padding: "2px 0" }}>No skill calls in this period</div>}
          </>
        )}
        {/* heatmap */}
        <SectionRule t={t} />
        <div style={{ marginBottom: 9 }}><Label t={t}>Daily activity</Label></div>
        <Heatmap days={heatmap} theme={t} accent={t.accent} />
        {/* footer note */}
        <div style={{ marginTop: 12, font: `500 8.5px ${t.mono}`, color: t.faint, textAlign: "center" }}>
          Est. cost via models.dev / LiteLLM · estimate
        </div>
        </div>{/* /scrolling body */}
      </div>
      {toast && (
        <div className="om-toast" style={{
          position: "absolute", top: 58, left: "50%", transform: "translateX(-50%)",
          zIndex: 20, whiteSpace: "nowrap", pointerEvents: "none",
          font: `600 12px ${t.mono}`, color: "#fff",
          background: toast.ok ? t.accent : "#e0795f",
          padding: "7px 13px", borderRadius: 9,
          boxShadow: "0 8px 22px rgba(0,0,0,0.34)",
        }}>
          {toast.msg}
        </div>
      )}
    </div>
  );
}

export default function App() {
  const [ws, setWs] = useState<Workspace | null>(null);
  // Which account tab is shown: "all" (aggregate) or an account id.
  const [activeTab, setActiveTab] = useState<string>("all");
  // User-chosen tab names, keyed by account id, overriding the auto-derived
  // default from each account's .claude.json. Persisted across restarts.
  const [labelOverrides, setLabelOverrides] = useState<Record<string, string>>(() => {
    try { return JSON.parse((typeof localStorage !== "undefined" && localStorage.getItem("tokenscope-account-labels")) || "{}"); }
    catch { return {}; }
  });
  const renameAccount = (id: string, label: string) => {
    setLabelOverrides((prev) => {
      const next = { ...prev };
      const trimmed = label.trim();
      // A blank name clears the override → falls back to the auto-derived name.
      if (trimmed) next[id] = trimmed; else delete next[id];
      try { localStorage.setItem("tokenscope-account-labels", JSON.stringify(next)); } catch {}
      return next;
    });
  };
  // Chosen color preset (persisted); orthogonal to Dark/Light/System.
  const [preset, setPreset] = useState<PresetId>(() => {
    const saved = typeof localStorage !== "undefined" ? localStorage.getItem("tokenscope-preset") : null;
    return (PRESETS.some((p) => p.id === saved) ? saved : "emerald") as PresetId;
  });
  const choosePreset = (id: PresetId) => {
    setPreset(id);
    try { localStorage.setItem("tokenscope-preset", id); } catch {}
  };
  // Period + which date we're viewing. refDate === null means "current/live"
  // (uses the workspace data + live updates); a set ISO date means we've
  // navigated to a past day/week/month, fetched on demand into `fetchedPeriod`.
  const [period, setPeriod] = useState<"Day" | "Week" | "Month">("Week");
  const [refDate, setRefDate] = useState<string | null>(null);
  const [fetchedPeriod, setFetchedPeriod] = useState<PeriodReport | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [openGen, setOpenGen] = useState(0);
  const [focused, setFocused] = useState(true); // browser preview: always "focused"
  // Theme preference: explicit Dark / Light, or System (follows the OS
  // appearance live on both macOS and Windows via prefers-color-scheme). First
  // run defaults to System.
  const [themePref, setThemePref] = useState<"dark" | "light" | "system">(() => {
    const saved = typeof localStorage !== "undefined" ? localStorage.getItem("tokenscope-theme") : null;
    if (saved === "dark" || saved === "light" || saved === "system") return saved;
    return "system";
  });
  const [systemDark, setSystemDark] = useState<boolean>(
    () => typeof window !== "undefined" && !!window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches
  );
  // Follow the OS appearance live while in System mode (and keep it current for
  // an instant switch back to System).
  useEffect(() => {
    if (typeof window === "undefined" || !window.matchMedia) return;
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const onChange = (e: MediaQueryListEvent) => setSystemDark(e.matches);
    mq.addEventListener("change", onChange);
    return () => mq.removeEventListener("change", onChange);
  }, []);
  const dark = themePref === "system" ? systemDark : themePref === "dark";
  // Cycle Dark → Light → System on each click; persist the choice.
  const cycleTheme = () =>
    setThemePref((p) => {
      const n = p === "dark" ? "light" : p === "light" ? "system" : "dark";
      try { localStorage.setItem("tokenscope-theme", n); } catch {}
      return n;
    });

  useEffect(() => {
    // Apply fresh data AND clear any stale error: a transient initial-load
    // failure must not pin the error page for the whole session — the next
    // successful fetch (focus refetch or the 30s background push) recovers it.
    const apply = (w: Workspace) => {
      setWs(w);
      setErr(null);
    };
    // initial load (shows the Loading state only until the first data arrives)
    fetchWorkspace().then(apply).catch((e) => setErr(String(e)));

    const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
    if (!inTauri) return;
    // Under StrictMode the effect mounts → cleans up → remounts; the async
    // listen()/onFocusChanged() promises can resolve after the first cleanup,
    // so unregister any late arrival immediately instead of leaking a duplicate.
    let dead = false;
    const unlisten: Array<() => void> = [];
    const track = (u: () => void) => {
      if (dead) u();
      else unlisten.push(u);
    };
    // live updates pushed from the background refresh thread — swaps the data in
    // place (no Loading), so values update without any flicker.
    listen<Workspace>("dashboard-updated", (e) => apply(e.payload)).then(track);
    // System appearance pushed natively from Rust (macOS). The webview's
    // prefers-color-scheme is unreliable for our hidden, non-activating menu-bar
    // panel, so the native event is the source of truth for System mode there;
    // it fires once at startup (correcting any stale launch value) and on every
    // OS theme change. Harmlessly never fires on Windows, where matchMedia works.
    listen<boolean>("system-theme", (e) => setSystemDark(e.payload)).then(track);
    // refetch the instant the popover gains focus (i.e. is opened)
    getCurrentWindow()
      .onFocusChanged(({ payload: focused }) => {
        setFocused(focused);
        if (focused) {
          setOpenGen((g) => g + 1); // re-run the count-up on each open
          fetchWorkspace().then(apply).catch(() => {});
        }
      })
      .then(track);
    return () => {
      dead = true;
      unlisten.forEach((u) => u());
    };
  }, []);

  // window is transparent; the rounded card paints its own background
  useEffect(() => {
    document.body.style.background = "transparent";
  }, [dark]);

  // Suppress per-property CSS transitions across a theme flip so the panel
  // repaints in the new theme in one step instead of cross-fading each color
  // (see .ts-no-transition in main.tsx). A background light→dark switch lands
  // while the panel is hidden; rAF callbacks don't run while hidden, so the
  // class stays on until the popover is shown — the first painted frame is
  // already the new theme with no transition, then we strip it a couple of
  // frames later so live interactions (e.g. switching the period) animate as
  // before. Skipped on the very first render (no prior frame to cross-fade).
  const firstThemeRun = useRef(true);
  useEffect(() => {
    if (firstThemeRun.current) {
      firstThemeRun.current = false;
      return;
    }
    const el = document.documentElement;
    el.classList.add("ts-no-transition");
    const id = requestAnimationFrame(() =>
      requestAnimationFrame(() => el.classList.remove("ts-no-transition"))
    );
    return () => cancelAnimationFrame(id);
  }, [dark]);

  // On-demand fetch when we've navigated to a past period (refDate set). Live
  // mode (refDate === null) reads the workspace + gets live pushes instead.
  useEffect(() => {
    if (refDate === null) return;
    let cancelled = false;
    fetchPeriod(activeTab, period, refDate)
      .then((r) => { if (!cancelled) setFetchedPeriod(r); })
      .catch(() => {});
    return () => { cancelled = true; };
  }, [refDate, period, activeTab]);

  const t = themeFor(dark, preset);
  if (err) {
    return <div style={{ padding: 20, font: `500 12px ${t.mono}`, color: "#e0795f" }}>Failed to load: {err}</div>;
  }
  if (!ws) {
    return (
      <div style={{ height: "100vh", padding: 10, boxSizing: "border-box", background: "transparent" }}>
        <div style={{ height: "100%", borderRadius: 14, background: dark ? "#1f2226" : "#ffffff",
          display: "flex", alignItems: "center", justifyContent: "center",
          font: `500 12px ${t.mono}`, color: t.dim }}>Loading…</div>
      </div>
    );
  }
  // Tabs only when there's more than one account (else the single account IS the
  // aggregate). "All" first so it's the default view.
  const tabs = ws.accounts.length > 1
    ? [{ id: "all", label: "All" }, ...ws.accounts.map((a) => ({ id: a.id, label: labelOverrides[a.id] ?? a.label }))]
    : [];
  // Selected dashboard; fall back to the aggregate if the active account is gone.
  const selected = activeTab === "all" ? ws.all : ws.accounts.find((a) => a.id === activeTab)?.dash;
  const dash = selected ?? ws.all;
  const effectiveTab = selected ? activeTab : "all";

  // The report to show: the live current period from the workspace, or the
  // on-demand fetched past period. `dash` here reflects the selected account.
  const liveReport = period === "Day" ? dash.day : period === "Month" ? dash.month : dash.week;
  const isCurrent = refDate === null;
  const report = isCurrent ? liveReport : (fetchedPeriod ?? liveReport);
  const loadingPeriod = !isCurrent && fetchedPeriod === null;

  // ── date navigation ──
  const goPrev = () => setRefDate(shiftPeriod(refDate ?? todayISO(), period, -1));
  const goNext = () => {
    const next = shiftPeriod(refDate ?? todayISO(), period, +1);
    setRefDate(isCurrentPeriod(next, period) ? null : next); // stepping onto the present → live
  };
  const goToday = () => setRefDate(null);
  const changePeriod = (p: string) => {
    setPeriod(p as "Day" | "Week" | "Month");
    setFetchedPeriod(null); // force a refetch at the new granularity
    if (refDate && isCurrentPeriod(refDate, p)) setRefDate(null);
  };
  const drillDay = (iso: string) => {
    if (!iso) return;
    setPeriod("Day");
    setFetchedPeriod(null);
    setRefDate(isCurrentPeriod(iso, "Day") ? null : iso);
  };
  const trendPick = (iso: string) => {
    if (!iso) return;
    setFetchedPeriod(null);
    setRefDate(isCurrentPeriod(iso, period) ? null : iso);
  };

  return (
    <Panel
      report={report}
      heatmap={dash.heatmap}
      period={period}
      onPeriod={changePeriod}
      dark={dark}
      themePref={themePref}
      onToggleTheme={cycleTheme}
      openGen={openGen}
      active={focused}
      tabs={tabs}
      activeTab={effectiveTab}
      onSelectTab={setActiveTab}
      onRename={renameAccount}
      preset={preset}
      onPickPreset={choosePreset}
      isCurrent={isCurrent}
      onPrev={goPrev}
      onNext={goNext}
      onToday={goToday}
      onDrillDay={drillDay}
      onTrendPick={trendPick}
      loading={loadingPeriod}
    />
  );
}
