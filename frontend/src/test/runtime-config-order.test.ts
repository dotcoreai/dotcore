import { describe, expect, it } from "vitest";

import indexHtml from "../../index.html?raw";

describe("runtime configuration loading", () => {
  it("loads config.js before the application module", () => {
    expect(indexHtml.indexOf('<script src="/config.js"></script>')).toBeGreaterThanOrEqual(0);
    expect(indexHtml.indexOf('<script src="/config.js"></script>')).toBeLessThan(
      indexHtml.indexOf('<script type="module" src="/src/main.tsx"></script>'),
    );
  });
});
