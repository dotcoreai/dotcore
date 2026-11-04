import { describe, expect, it } from "vitest";

import { cn } from "@/lib/utils";

describe("cn density size names", () => {
  it("lets a caller's override replace a density class", () => {
    expect(cn("h-control-md", "h-auto")).toBe("h-auto");
    expect(cn("size-control-md", "size-11")).toBe("size-11");
    expect(cn("min-h-menu-row", "min-h-0")).toBe("min-h-0");
  });

  it("lets a density class replace a numeric class", () => {
    expect(cn("h-8", "h-control-lg")).toBe("h-control-lg");
  });

  it("keeps different properties", () => {
    expect(cn("h-control-md", "min-h-menu-row")).toBe("h-control-md min-h-menu-row");
  });
});
