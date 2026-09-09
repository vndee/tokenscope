import { describe, it, expect } from "vitest";
import { fmtCountdown, fmtAge, quotaFailMessage, QUOTA_FAIL_KINDS } from "./data";

// A fixed instant, so no case depends on when the suite runs. The unit the
// panel cares about is the gap between the two arguments, not either one.
const NOW = Date.UTC(2026, 8, 9, 8, 0, 0);
// `resetsAt` is unix seconds on both agents; `nowMs` is millis.
const secondsFromNow = (n: number) => Math.floor(NOW / 1000) + n;

describe("fmtCountdown", () => {
  it("reads as hours and minutes within a day", () => {
    expect(fmtCountdown(secondsFromNow(2 * 3600 + 14 * 60), NOW)).toBe("2h 14m");
    // The shape a weekly window usually lands on.
    expect(fmtCountdown(secondsFromNow(9 * 3600 + 8 * 60), NOW)).toBe("9h 8m");
  });

  it("reads as days and hours beyond a day", () => {
    expect(fmtCountdown(secondsFromNow(4 * 86400 + 6 * 3600), NOW)).toBe("4d 6h");
  });

  it("drops a trailing zero unit rather than padding it", () => {
    // "2h 0m" and "3d 0h" say nothing the shorter form does not.
    expect(fmtCountdown(secondsFromNow(2 * 3600), NOW)).toBe("2h");
    expect(fmtCountdown(secondsFromNow(3 * 86400), NOW)).toBe("3d");
  });

  it("is minutes only under an hour", () => {
    expect(fmtCountdown(secondsFromNow(30 * 60), NOW)).toBe("30m");
    expect(fmtCountdown(secondsFromNow(59 * 60), NOW)).toBe("59m");
  });

  it("never rounds a reset that is seconds away down to 0m", () => {
    // "0m" would read as "already there" while there is still time left.
    expect(fmtCountdown(secondsFromNow(45), NOW)).toBe("<1m");
    expect(fmtCountdown(secondsFromNow(1), NOW)).toBe("<1m");
  });

  it("yields nothing once the moment has passed", () => {
    // An elapsed countdown is a different thing to say, not a small number, so
    // the caller words it. A reading minutes old makes this ordinary, not an
    // error — hence null rather than a negative or a throw.
    expect(fmtCountdown(secondsFromNow(0), NOW)).toBeNull();
    expect(fmtCountdown(secondsFromNow(-120), NOW)).toBeNull();
    expect(fmtCountdown(secondsFromNow(-9 * 86400), NOW)).toBeNull();
  });

  it("counts from the moment given, not from the wall clock", () => {
    // The panel renders a reading that may be minutes old; the countdown must
    // follow the instant it is handed. Same reset, two different `now`s.
    const reset = secondsFromNow(3 * 3600);
    expect(fmtCountdown(reset, NOW)).toBe("3h");
    expect(fmtCountdown(reset, NOW + 60 * 60 * 1000)).toBe("2h");
  });
});

describe("fmtAge", () => {
  const NOW_MS = Date.UTC(2026, 8, 9, 12, 0, 0);
  const agoMins = (m: number) => NOW_MS - m * 60_000;

  it("calls anything under a minute just now", () => {
    expect(fmtAge(NOW_MS, NOW_MS)).toBe("just now");
    expect(fmtAge(agoMins(0.4), NOW_MS)).toBe("just now");
  });

  it("counts minutes below an hour and hours above it", () => {
    expect(fmtAge(agoMins(12), NOW_MS)).toBe("12m ago");
    expect(fmtAge(agoMins(59), NOW_MS)).toBe("59m ago");
    expect(fmtAge(agoMins(120), NOW_MS)).toBe("2h ago");
  });

  it("never reads as the future when a clock disagrees", () => {
    // Timestamps come from the backend; a moment slightly ahead of the
    // frontend's clock must not render as a negative age.
    expect(fmtAge(NOW_MS + 5_000, NOW_MS)).toBe("just now");
  });
});

describe("quotaFailMessage", () => {
  it("gives every known kind its own sentence", () => {
    const seen = new Set<string>();
    for (const kind of QUOTA_FAIL_KINDS) {
      const msg = quotaFailMessage(kind);
      expect(msg, `${kind} needs a sentence`).toBeTruthy();
      expect(seen.has(msg), `${kind} repeats another kind's sentence`).toBe(false);
      seen.add(msg);
    }
    expect(seen.size).toBe(QUOTA_FAIL_KINDS.length);
  });

  it("names being signed out plainly, since that is the actionable one", () => {
    expect(quotaFailMessage("signed-out")).toMatch(/signed out/i);
  });

  it("falls back rather than showing nothing for a kind it does not know", () => {
    // The Rust enum is the source of truth and can gain a variant without this
    // file hearing about it. An unknown tag must still produce a sentence.
    expect(quotaFailMessage("some-future-kind")).toBeTruthy();
  });
});
