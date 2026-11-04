import { describe, expect, it } from "vitest";

import type { ProfileRow } from "./catalog-model-row";
import { runsOnField } from "./catalog-profile-fields";
import { catalogViewCodec } from "./catalog-view-state";
import { profileTableStart, startShows } from "./profile-table-seed";

// Fast is a llama.cpp profile with image input that runs on node-a; Quality is a vLLM profile that
// runs nowhere.
const fast = profile("fast", { engineKinds: ["llama-cpp"], imageInput: true });
const quality = profile("quality", { engineKinds: ["vllm"], quantization: "NVFP4" });

// This site and node-a, as a read evaluating node-a offers them.
const runsOn = runsOnField(
  [
    { label: "this site", scope: { kind: "site" } },
    { label: "node-a", scope: { kind: "node", siteNodeId: "node-a" } },
  ],
  (row) => row.profileId === "fast",
);

function chips(search: string) {
  return catalogViewCodec.parse(search).predicates;
}

describe("profileTableStart", () => {
  it("copies the profile chips and the Runs on values, not the search, sort or columns", () => {
    const catalog = chips(
      "q=gemma&f=runs,engine,cap,quant,spec&runs=node%3Anode-a&engine=llama-cpp&cap=image&quant=Q4_K_M&spec=MTP&sort=-memory&cols=memory",
    );

    expect(profileTableStart(catalog, runsOn)).toEqual([
      { field: "runsOn", values: ["node:node-a"] },
      { field: "engine", values: ["llama-cpp"] },
      { field: "capability", values: ["image"] },
      { field: "quantization", values: ["Q4_K_M"] },
      { field: "speculative", values: ["MTP"] },
    ]);
  });

  it("starts at this site by default", () => {
    expect(profileTableStart(chips(""), runsOn)).toEqual([{ field: "runsOn", values: ["site"] }]);
  });

  it("leads with Runs on, as the table adds it", () => {
    expect(profileTableStart(chips("f=engine,runs&engine=vllm&runs=site"), runsOn)).toEqual([
      { field: "runsOn", values: ["site"] },
      { field: "engine", values: ["vllm"] },
    ]);
  });

  it("drops machines the read doesn't offer, falling back to this site", () => {
    expect(profileTableStart(chips("f=runs&runs=node%3Anode-a&runs=node%3Agone"), runsOn)).toEqual([
      { field: "runsOn", values: ["node:node-a"] },
    ]);
    expect(profileTableStart(chips("f=runs&runs=node%3Agone"), runsOn)).toEqual([
      { field: "runsOn", values: ["site"] },
    ]);
  });

  it("starts without Runs on when the catalog has no chip or the read doesn't offer it yet", () => {
    expect(profileTableStart(chips("f=engine&engine=vllm"), runsOn)).toEqual([
      { field: "engine", values: ["vllm"] },
    ]);
    expect(profileTableStart(chips("f="), runsOn)).toEqual([]);
    expect(profileTableStart(chips("f=runs,engine&runs=site&engine=vllm"), undefined)).toEqual([
      { field: "engine", values: ["vllm"] },
    ]);
  });

  it("drops the chips that would hide the named profile", () => {
    const catalog = chips("f=runs,engine,cap&runs=site&engine=llama-cpp&cap=image");

    expect(profileTableStart(catalog, runsOn, fast)).toEqual([
      { field: "runsOn", values: ["site"] },
      { field: "engine", values: ["llama-cpp"] },
      { field: "capability", values: ["image"] },
    ]);
    // Quality runs nowhere and is neither llama.cpp nor image capable.
    expect(profileTableStart(catalog, runsOn, quality)).toEqual([]);
  });
});

describe("startShows", () => {
  it("judges the profile chips, and Runs on once its field is known", () => {
    const start = profileTableStart(chips("f=runs,engine&runs=site&engine=llama-cpp"), runsOn);

    expect(startShows(start, fast, runsOn)).toBe(true);
    expect(startShows(start, quality, runsOn)).toBe(false);
    expect(startShows([{ field: "runsOn", values: ["site"] }], quality)).toBe(true);
  });
});

function profile(profileId: string, values: Partial<ProfileRow>): ProfileRow {
  return {
    acceleratorRequirements: [],
    configurationId: "config",
    displayName: profileId,
    engineKinds: [],
    hardwareClassEstimates: [],
    imageInput: false,
    modelId: "alpha",
    platforms: [],
    profileId,
    scenarioUnsupportedClasses: [],
    speculativeOptions: [],
    ...values,
  };
}
