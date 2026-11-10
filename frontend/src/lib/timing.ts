/**
 * Every delay the console waits on before acting: UI delays that keep feedback from flashing or
 * reacting to a passing pointer, and the cadences of retries, polls and reconnects. Code reads
 * them through `timing()` when it schedules a timer, so one place sets them all.
 */
export interface Timing {
  /** How long a pending read waits before its loading feedback shows. */
  loadingRevealMs: number;
  /** How long loading feedback stays once shown, so it never blinks. */
  loadingMinimumVisibleMs: number;
  /**
   * How long the pointer rests on the collapsed sidebar before it peeks open, so a graze on the way
   * elsewhere doesn't open it.
   */
  sidebarPeekEnterMs: number;
  /**
   * How long the peeked sidebar stays open after the pointer leaves, longer than the enter delay so
   * a pointer heading for a revealed sub-item doesn't close it mid-transit.
   */
  sidebarPeekLeaveMs: number;
  /** How long typing pauses before a typed sizing scenario applies, so reads don't follow keys. */
  scenarioInputMs: number;
  /** How long the pointer rests on a tooltip's trigger before the tooltip opens. */
  tooltipOpenMs: number;
  /** How long copy feedback stays on a copy control. */
  copyFeedbackMs: number;
  /**
   * Backoff after failed polled reads and dropped streams, one entry per consecutive failure; the
   * last one repeats. A stream open past the last entry counts as healthy again.
   */
  pollRetryMs: readonly number[];
  /** Backoff before a log viewer reconnects a dropped log stream; the last entry repeats. */
  logReconnectMs: readonly number[];
  /**
   * How long the resource watch waits to reopen its stream after kinds are only removed, so a kind
   * released and registered again in quick succession keeps the open stream.
   */
  resourceWatchReopenMs: number;
  /**
   * How often an open view re-reads profile compatibility; shorter than the backend's expiry of
   * target observations, so verdicts stay current.
   */
  profileCompatibilityRefreshMs: number;
}

/** The console's timing in production. */
export const productionTiming: Timing = Object.freeze({
  loadingRevealMs: 300,
  loadingMinimumVisibleMs: 200,
  sidebarPeekEnterMs: 150,
  sidebarPeekLeaveMs: 300,
  scenarioInputMs: 500,
  tooltipOpenMs: 600,
  copyFeedbackMs: 1_500,
  pollRetryMs: Object.freeze([1_000, 2_000, 5_000]),
  logReconnectMs: Object.freeze([250, 500, 1_000, 2_000, 4_000, 8_000]),
  resourceWatchReopenMs: 100,
  profileCompatibilityRefreshMs: 5_000,
});

/** The timing in effect. */
export function timing(): Timing {
  return current;
}

/** Replaces the timing in effect, for timers scheduled from now on. */
export function setTiming(next: Timing): void {
  current = next;
}

let current = productionTiming;
