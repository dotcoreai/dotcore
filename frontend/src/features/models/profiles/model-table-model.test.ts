import { describe, expect, it } from "vitest";

import {
  nvidiaEstimates,
  syntheticCatalog,
} from "@/features/models/catalog/catalog-table-test-support";
import {
  catalogViewCodec,
  type CatalogViewState,
} from "@/features/models/catalog/catalog-view-state";

import { type ModelRow, type ProfileMemory, type ProfileRow, projectModelRows } from "./model-row";
import {
  buildCatalogTable,
  formatContext,
  formatMemory,
  formatMemoryFit,
} from "./model-table-model";

const catalog = syntheticCatalog();

function synthetic(estimates: Record<string, bigint> = {}): ModelRow[] {
  return projectModelRows({
    deployments: [],
    engineReleases: catalog.engineReleases,
    hardwareClasses: catalog.hardwareClasses,
    estimates: nvidiaEstimates(
      Object.entries(estimates).map(([key, bytes]) => {
        const [modelId, profileId] = key.split("/");
        return [modelId, profileId, bytes] as const;
      }),
    ),
    models: catalog.models,
  });
}

function view(search: string): CatalogViewState {
  return catalogViewCodec.parse(search);
}

function build(rows: ModelRow[], search = "", estimatesPending = false) {
  return buildCatalogTable({ estimatesPending, rows, view: view(search) });
}

function shownProfiles(table: ReturnType<typeof build>, modelId: string): string[] {
  return table.rows.find((row) => row.row.modelId === modelId)!.children.map((p) => p.profileId);
}

describe("per-profile filtering", () => {
  it("matches Engine vLLM plus a quantization only llama.cpp profiles use against nothing", () => {
    const table = build(synthetic(), "?engine=vllm&quant=Q4_K_M");

    expect(table.rows).toEqual([]);
    expect(table.shownCount).toBe(0);
    expect(table.totalCount).toBe(2);
  });

  it("shows a model through one matching profile with the hidden count", () => {
    const table = build(synthetic(), "?engine=vllm&quant=NVFP4");

    expect(table.rows.map((row) => row.row.modelId)).toEqual(["alpha-7b"]);
    const [alpha] = table.rows;
    expect(alpha.children.map((p) => p.profileId)).toEqual(["sized"]);
    expect(alpha.hiddenChildCount).toBe(4);
    expect(alpha.expandable).toBe(true);
    expect(alpha.engineKinds).toEqual(["vllm"]);
    expect(alpha.quantizations).toEqual(["NVFP4"]);
    expect(table.shownCount).toBe(1);
  });

  it("keeps the profiles offering a selected speculative method, whatever their default", () => {
    const table = build(synthetic(), "?spec=MTP");

    expect(table.rows.map((row) => row.row.modelId)).toEqual(["alpha-7b"]);
    expect(shownProfiles(table, "alpha-7b")).toEqual(["fast", "quality"]);
  });

  it("requires every selected capability and accepts any selected engine", () => {
    expect(shownProfiles(build(synthetic(), "?cap=image"), "alpha-7b")).toEqual([
      "fast",
      "quality",
    ]);
    expect(shownProfiles(build(synthetic(), "?engine=vllm&engine=llama-cpp"), "alpha-7b")).toEqual([
      "sized",
      "cpu",
      "fast",
      "quality",
      "orphan",
    ]);
  });
});

describe("summaries over matching profiles", () => {
  const estimates = { "alpha-7b/fast": 7_500_000_000n, "alpha-7b/quality": 10_000_000_000n };

  it("recomputes ranges over the matching profiles", () => {
    const all = build(synthetic(estimates)).rows[0];
    const llama = build(synthetic(estimates), "?engine=llama-cpp").rows[0];

    expect(all.memory).toEqual({
      kinds: ["estimate", "minimum", "system-ram"],
      max: 24_000_000_000,
      min: 6_000_000_000,
    });
    // The vLLM configuration lowers the model's maximum context, so the model spans both.
    expect(all.maximumContext).toEqual({ max: 262144, min: 65536 });
    expect(llama.memory).toEqual({
      kinds: ["estimate", "system-ram"],
      max: 16_000_000_000,
      min: 7_500_000_000,
    });
    expect(llama.maximumContext).toEqual({ max: 262144, min: 262144 });
    expect(llama.hiddenChildCount).toBe(2);
  });

  it("ranges over a mix of estimates and minimums", () => {
    const mixed = build(synthetic(estimates), "?quant=Q8_0").rows[0];

    expect(shownProfiles(build(synthetic(estimates), "?quant=Q8_0"), "alpha-7b")).toEqual([
      "quality",
      "orphan",
    ]);
    expect(mixed.memory).toEqual({
      kinds: ["estimate", "minimum"],
      max: 10_000_000_000,
      min: 6_000_000_000,
    });
  });

  it("leaves memory and context absent when no matching profile has them", () => {
    const bare = build(synthetic(), "?q=bare").rows[0];

    expect(bare.memory).toBeUndefined();
    expect(bare.maximumContext).toBeUndefined();
    expect(bare.totalContext).toBeUndefined();
  });
});

describe("facet counts", () => {
  it("counts models with a profile that carries the value and matches every other predicate", () => {
    const { facets } = build(synthetic(), "?engine=vllm");

    // The engine facet ignores its own predicate; quantization and capability apply the engine one.
    expect(facets.engine).toEqual([
      { count: 1, label: "llama.cpp", value: "llama-cpp" },
      { count: 2, label: "vLLM", value: "vllm" },
    ]);
    expect(facets.quantization).toEqual([
      { count: 1, label: "NVFP4", value: "NVFP4" },
      { count: 0, label: "Q4_K_M", value: "Q4_K_M" },
      { count: 1, label: "Q8_0", value: "Q8_0" },
    ]);
    // Image input is the only capability; long context is no longer one.
    expect(facets.capability).toEqual([{ count: 0, label: "Image input", value: "image" }]);
    expect(facets.speculative).toEqual([{ count: 0, label: "MTP", value: "MTP" }]);
    expect(build(synthetic()).facets.speculative).toEqual([
      { count: 1, label: "MTP", value: "MTP" },
    ]);
  });
});

describe("search", () => {
  it("matches the display name and the model id, ignoring case", () => {
    const rows = [row("qwen-27b", "Fancy Model"), row("other", "Plain")];

    expect(build(rows, "?q=FANCY").rows.map((r) => r.row.modelId)).toEqual(["qwen-27b"]);
    expect(build(rows, "?q=qwen").rows.map((r) => r.row.modelId)).toEqual(["qwen-27b"]);
    expect(build(rows, "?q=nothing").rows).toEqual([]);
    expect(build(rows, "?q=nothing").totalCount).toBe(2);
  });
});

describe("sorting", () => {
  const rows = [
    row("zeta", "Zeta", [
      profile("z1", { totalContext: 8000, memory: est(20) }),
      profile("z2", { totalContext: 32000, memory: est(10) }),
    ]),
    row("alpha", "Alpha", [profile("a1", { totalContext: 4000, memory: min(15) })]),
    row("mid", "Mid", [profile("m1", {})]),
    row("beta", "Beta", [
      profile("b1", { totalContext: 16000, memory: est(12) }),
      profile("b2", { totalContext: 64000, memory: est(30) }),
      profile("b3", { memory: est(20) }),
    ]),
  ];

  function order(search: string, estimatesPending = false) {
    return build(rows, search, estimatesPending).rows.map((r) => r.row.modelId);
  }

  it("sorts by model name in both directions", () => {
    expect(order("")).toEqual(["alpha", "beta", "mid", "zeta"]);
    expect(order("?sort=-name")).toEqual(["zeta", "mid", "beta", "alpha"]);
  });

  it("sorts memory ascending by the lowest and descending by the highest matching value", () => {
    expect(order("?sort=memory")).toEqual(["zeta", "beta", "alpha", "mid"]);
    expect(order("?sort=-memory")).toEqual(["beta", "zeta", "alpha", "mid"]);
  });

  it("sorts total context ascending by the lowest and descending by the highest matching value", () => {
    expect(order("?sort=total-context")).toEqual(["alpha", "zeta", "beta", "mid"]);
    expect(order("?sort=-total-context")).toEqual(["beta", "zeta", "alpha", "mid"]);
  });

  it("orders the profiles of a model by memory, largest first, and by the sorted value", () => {
    // Model rows reverse under a descending name sort; their profiles keep their default order.
    const profiles = (search: string) =>
      build(rows, search)
        .rows.find((r) => r.row.modelId === "beta")!
        .children.map((p) => p.profileId);

    expect(profiles("")).toEqual(["b2", "b3", "b1"]);
    expect(profiles("?sort=-name")).toEqual(["b2", "b3", "b1"]);
    expect(profiles("?sort=memory")).toEqual(["b1", "b3", "b2"]);
    expect(profiles("?sort=-memory")).toEqual(["b2", "b3", "b1"]);
    expect(profiles("?sort=total-context")).toEqual(["b1", "b2", "b3"]);
    expect(profiles("?sort=-total-context")).toEqual(["b2", "b1", "b3"]);
  });

  it("sorts intelligence by the model's score, unscored models last in both directions", () => {
    const scored = [
      row("low", "Low", [
        profile("l1", { referenceScore: 9 }),
        profile("l2", { referenceScore: 9 }),
      ]),
      row("none", "Aardvark", [profile("n1", {})]),
      row("high", "High", [profile("h1", { referenceScore: 34 })]),
      row("mid", "Mid", [profile("m1", { referenceScore: 14 })]),
      row("bare", "Bare"),
    ];
    const ordered = (search: string) => build(scored, search).rows.map((r) => r.row.modelId);

    expect(ordered("?sort=intelligence")).toEqual(["low", "mid", "high", "none", "bare"]);
    expect(ordered("?sort=-intelligence")).toEqual(["high", "mid", "low", "none", "bare"]);
  });

  it("keeps the name order while memory estimates load", () => {
    expect(order("?sort=-memory", true)).toEqual(["alpha", "beta", "mid", "zeta"]);
    expect(build(rows, "?sort=-memory", true).memoryPending).toBe(true);
    expect(build(rows, "?sort=-memory", false).memoryPending).toBe(false);
  });
});

function row(modelId: string, displayName: string, profiles: ProfileRow[] = []): ModelRow {
  return {
    colorKey: modelId,
    displayName,
    modelId,
    profileCount: profiles.length,
    profiles: profiles.map((candidate) => ({ ...candidate, modelId })),
    runningCount: 0,
    runningStatus: undefined,
    speculativeMethods: [],
    requestKinds: [],
    supportsVision: false,
  };
}

function profile(profileId: string, overrides: Partial<ProfileRow>): ProfileRow {
  return {
    acceleratorRequirements: [],
    configurationId: "config",
    displayName: profileId,
    engineKinds: ["llama-cpp"],
    hardwareClassEstimates: [],
    scenarioUnsupportedClasses: [],
    imageInput: false,
    modelId: "model",
    platforms: [],
    profileId,
    speculativeOptions: [],
    ...overrides,
  };
}

function est(gigabytes: number): ProfileMemory {
  return { kind: "estimate", max: gigabytes * 1e9, min: gigabytes * 1e9 };
}

function min(gigabytes: number): ProfileMemory {
  return { kind: "minimum", max: gigabytes * 1e9, min: gigabytes * 1e9 };
}

describe("value formatting", () => {
  it("formats memory, marking minimums and labeling system RAM only", () => {
    expect(formatMemory({ kind: "estimate", max: 23_500_000_000, min: 23_500_000_000 })).toEqual({
      value: "21.9 GiB",
    });
    expect(formatMemory({ kind: "minimum", max: 10_000_000_000, min: 10_000_000_000 })).toEqual({
      value: "≥9.3 GiB",
    });
    expect(formatMemory({ kind: "system-ram", max: 16_000_000_000, min: 16_000_000_000 })).toEqual({
      label: "system RAM",
      value: "14.9 GiB",
    });
    expect(
      formatMemory({ kinds: ["estimate", "minimum"], max: 32_000_000_000, min: 16_240_000_000 }),
    ).toEqual({ value: "15.1–29.8 GiB" });
    expect(
      formatMemory({ kinds: ["estimate", "system-ram"], max: 32_000_000_000, min: 16_240_000_000 }),
    ).toEqual({ value: "15.1–29.8 GiB" });
    expect(formatMemory(undefined)).toEqual({ value: "—" });
  });

  it("names free memory in a memory fit on free now, and the total plainly", () => {
    expect(formatMemoryFit("exceeds", "free")).toBe("exceeds free memory");
    expect(formatMemoryFit("exceeds", "total")).toBe("exceeds memory");
    expect(formatMemoryFit("fits", "free")).toBe(formatMemoryFit("fits", "total"));
  });

  it("formats context token counts in k tokens", () => {
    expect(formatContext(32_768)).toBe("32k");
    expect(formatContext(131_072)).toBe("128k");
    expect(formatContext(30_000)).toBe("30k");
    expect(formatContext({ max: 250_000, min: 32_768 })).toBe("32k–250k");
    expect(formatContext({ max: 32_768, min: 32_768 })).toBe("32k");
    expect(formatContext(undefined)).toBe("—");
  });
});
