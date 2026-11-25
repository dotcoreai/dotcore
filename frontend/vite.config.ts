import { fileURLToPath, URL } from "node:url";

import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { configDefaults, defineConfig } from "vitest/config";

import { getDevServerConfig } from "./tools/vite-dev-server";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": fileURLToPath(new URL("./src", import.meta.url)),
    },
  },
  server: getDevServerConfig(),
  test: {
    environment: "jsdom",
    pool: "threads",
    // One environment per worker instead of one per file: setting it up was a third of the
    // suite's time. Shared setup and the restore options below clear what a test leaves behind.
    isolate: false,
    unstubEnvs: true,
    unstubGlobals: true,
    restoreMocks: true,

    // Vitest's default fake list minus `setInterval`: jsdom starts its animation frame loop once on
    // the current `setInterval`, and if that is a fake one, every later frame in the worker stalls.
    fakeTimers: {
      toFake: [
        "setTimeout",
        "clearTimeout",
        "setImmediate",
        "clearImmediate",
        "Date",
        "requestAnimationFrame",
        "cancelAnimationFrame",
        "requestIdleCallback",
        "cancelIdleCallback",
        "hrtime",
        "performance",
      ],
    },

    exclude: [...configDefaults.exclude, "tools/**/*.test.mjs"],
    setupFiles: ["./src/test/setup.ts"],
    // The endpoint tests re-import their client via `vi.resetModules()` + `await import()`,
    // which pulls the real @connectrpc/connect through `importOriginal`. Under parallel-worker
    // load in CI that import can exceed the default 5s test timeout and flake a correct test.
    // Raise it suite-wide so load-induced slowness doesn't fail a test; a test that truly hangs
    // still fails, just later.
    testTimeout: 15000,
    coverage: {
      provider: "v8",
      reporter: ["text", "html"],
    },
  },
});
