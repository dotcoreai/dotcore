import { describe, expect, it } from "vitest";

import { machineNow } from "./machine-site-values";
import { device, facts, grid, node } from "./machine-site-values-test-support";

describe("machine now readings", () => {
  it("never reads an open step, however recent", () => {
    const { machines, site } = machineNow(
      { cpu: grid(node("big", [0.4, 0.5, 0.99], [2]), node("small", [0.2, 0.3, 0.98], [2])) },
      facts,
    );

    // Step 2 is open for both machines; the newest closed step is step 1.
    expect(machines.get("big")?.cpu).toBe(0.5);
    expect(site.cpu).toBeCloseTo((0.5 * 48 + 0.3 * 16) / 64);
  });

  it("skips a step still open on one machine for the site", () => {
    const { site } = machineNow(
      { power: grid(node("big", [100, 100, 100]), node("small", [10, 10, 90], [2])) },
      facts,
    );

    expect(site.power).toBe(110);
  });

  it("reads the hottest GPU as the site temperature and sums used storage", () => {
    const { site } = machineNow(
      {
        storageSize: grid(node("big", [1000, 1000]), node("small", [200, 200])),
        storageUsed: grid(node("big", [100, 100]), node("small", [100, 100])),
        temperature: grid(node("big", [60, 71]), node("small", [66, 64])),
      },
      facts,
    );

    expect(site.temperature).toBe(71);
    expect(site.storageUsed).toBe(200);
    expect(site.storageSize).toBe(1200);
  });

  it("leaves unified GPU memory out of VRAM and reads used RAM", () => {
    const { machines, site } = machineNow(
      {
        ramAvailable: grid(node("big", [40, 30])),
        ramTotal: grid(node("big", [64, 64])),
        vramTotal: grid(device("big", "gpu0", [24, 24]), device("big", "gpu-unified", [900, 900])),
        vramUsed: grid(device("big", "gpu0", [10, 12]), device("big", "gpu-unified", [500, 500])),
      },
      facts,
    );

    expect(site.vramUsed).toBe(12);
    expect(site.vramTotal).toBe(24);
    expect(machines.get("big")).toMatchObject({ ramTotal: 64, ramUsed: 34 });
  });

  it("reads nothing when no closed step is fresh", () => {
    const { machines, site } = machineNow(
      { cpu: grid(node("big", [0.4, null, null, null, null, null, null], undefined)) },
      facts,
    );

    expect(site.cpu).toBeUndefined();
    expect(machines.get("big")?.cpu).toBeUndefined();
  });
});
