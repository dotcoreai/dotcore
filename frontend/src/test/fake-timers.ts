import { onTestFinished, vi } from "vitest";

/**
 * Runs the current test on fake timers that testing-library's waits and `findBy*` queries
 * advance, so a test can wait on rendered state while it controls the clock. Real timers come
 * back when the test finishes.
 */
export function useFakeTimersWithWaits() {
  vi.useFakeTimers();
  // testing-library advances fake timers in its waits only when it finds a `jest` global.
  vi.stubGlobal("jest", { advanceTimersByTime: (ms: number) => vi.advanceTimersByTime(ms) });
  onTestFinished(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });
}
