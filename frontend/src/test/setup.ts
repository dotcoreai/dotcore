import "@testing-library/jest-dom/vitest";

import { configure } from "@testing-library/dom";
import { cleanup } from "@testing-library/react";
import { afterEach, beforeEach, vi } from "vitest";

import { setTiming, type Timing } from "@/lib/timing";

// testing-library's default async-utility timeout (asyncUtilTimeout, 1s) is too tight for async
// UI callbacks when the suite runs under parallel-worker load in CI; a callback that lands just
// over 1s flakes the test even though it is correct. Raise the default so these waits tolerate
// load-induced slowness. This only gives assertions more time to pass; a callback that never
// fires still fails.
configure({ asyncUtilTimeout: 4000 });

// Tests run without UI delays, and retries and reconnects come quickly without spinning. A steady
// refresh waits on nothing, so it never comes within a test. A test whose behavior depends on a
// delay or a refresh sets the production timing for itself.
const testTiming: Timing = Object.freeze({
  loadingRevealMs: 0,
  loadingMinimumVisibleMs: 0,
  sidebarPeekEnterMs: 0,
  sidebarPeekLeaveMs: 0,
  scenarioInputMs: 0,
  tooltipOpenMs: 0,
  copyFeedbackMs: 0,
  pollRetryMs: Object.freeze([10]),
  logReconnectMs: Object.freeze([10]),
  resourceWatchReopenMs: 10,
  // An hour, which no test outlives; a delay past 2^31-1 ms would fire at once instead.
  profileCompatibilityRefreshMs: 60 * 60 * 1_000,
  recommendationReadMs: 10,
});

setTiming(testTiming);

// jsdom's viewport width, which puts responsive components on their desktop layout.
const JSDOM_DEFAULT_WIDTH = 1024;

afterEach(() => {
  cleanup();
  setTiming(testTiming);
  // Files share one environment, so state a test leaves on the window reaches the next file.
  delete window.__DOTCORE_CONFIG__;
  Object.defineProperty(window, "innerWidth", {
    configurable: true,
    value: JSDOM_DEFAULT_WIDTH,
    writable: true,
  });
});

// The sidebar primitive's useIsMobile hook reads window.matchMedia, which jsdom does not
// implement. Stubbed globally because the console shell renders the sidebar on every route, so
// any test that mounts a route through the router or the layout needs this. jsdom's default
// 1024px innerWidth already puts the primitive on its desktop branch regardless of `matches`.
beforeEach(() => {
  window.matchMedia = vi.fn().mockImplementation((query: string) => ({
    matches: false,
    media: query,
    onchange: null,
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    dispatchEvent: vi.fn(),
  }));
});
