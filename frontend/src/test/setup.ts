import "@testing-library/jest-dom/vitest";

import { configure } from "@testing-library/dom";
import { cleanup } from "@testing-library/react";
import { afterEach, beforeEach, vi } from "vitest";

// testing-library's default async-utility timeout (asyncUtilTimeout, 1s) is too tight for async
// UI callbacks when the suite runs under parallel-worker load in CI; a callback that lands just
// over 1s flakes the test even though it is correct. Raise the default so these waits tolerate
// load-induced slowness. This only gives assertions more time to pass; a callback that never
// fires still fails.
configure({ asyncUtilTimeout: 4000 });

afterEach(() => {
  cleanup();
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
