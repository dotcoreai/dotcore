import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import type { ModelRow, ProfileRow } from "@/features/models/profiles/model-row";
import { buildCatalogTable } from "@/features/models/profiles/model-table-model";
import { catalogLocation, catalogRunsOn } from "@/features/models/profiles/profile-availability";
import { indexProfileCompatibility } from "@/features/models/profiles/profile-compatibility";
import {
  compatibilityResponse,
  driverTooOld,
  notReporting,
  targetCompatibility,
} from "@/features/models/profiles/profile-compatibility-test-support";
import { MemoryFit } from "@/gen/dotcore/deployment/v1/memory_fit_pb";
import {
  ListModelProfileCompatibilityResponseSchema,
  SizingScenarioUnsupportedReason,
} from "@/gen/dotcore/model/v1/service_pb";

import { catalogViewCodec, type CatalogViewState } from "./catalog-view-state";

// Alpha runs on the working machine with one of two profiles, Beta can't run there, Gamma runs
// there, and the second machine isn't reporting.
const working = { executionTargetId: "pool", siteNodeId: "node-a" };
const silent = { executionTargetId: "pool", siteNodeId: "node-b" };

const rows: ModelRow[] = [
  row("alpha", "Alpha", 40, ["a1", "a2"]),
  row("beta", "Beta", 70, ["b1"]),
  row("gamma", "Gamma", undefined, ["g1"]),
];

function read() {
  const parts = [
    compatibilityResponse(
      "alpha",
      targetCompatibility(working, { a1: {}, a2: { reason: driverTooOld } }),
      notReporting(silent),
    ),
    compatibilityResponse(
      "beta",
      targetCompatibility(working, { b1: { reason: driverTooOld } }),
      notReporting(silent),
    ),
    compatibilityResponse("gamma", targetCompatibility(working, { g1: {} }), notReporting(silent)),
  ];
  return indexProfileCompatibility(
    create(ListModelProfileCompatibilityResponseSchema, {
      entries: parts.flatMap(({ entries }) => entries),
      targets: parts[0].targets,
    }),
  );
}

function build(search: string, withField = true, estimatesPending = false) {
  const runsOn = withField ? catalogRunsOn(read(), []) : undefined;
  return buildCatalogTable({
    estimatesPending,
    rows,
    runsOn,
    view: catalogViewCodec.parse(search),
  });
}

function shown(table: ReturnType<typeof build>): Record<string, string[]> {
  return Object.fromEntries(
    table.rows.map((entry) => [entry.row.modelId, entry.children.map((p) => p.profileId)]),
  );
}

describe("Catalog Runs on", () => {
  it("shows only the models and profiles that run on the site by default", () => {
    const table = build("");

    expect(shown(table)).toEqual({ alpha: ["a1"], gamma: ["g1"] });
    expect(table.rows.find((entry) => entry.row.modelId === "alpha")?.hiddenChildCount).toBe(1);
    expect(table.shownCount).toBe(2);
  });

  it("lists this site and each evaluated machine with the models that run there", () => {
    const { runsOn } = build("").facets;

    expect(runsOn).toEqual([
      { count: 2, label: "this site", value: "site" },
      { count: 2, label: "node-a", value: "node:node-a" },
      { count: 0, label: "node-b", value: "node:node-b" },
    ]);
  });

  it("narrows to a machine, and a machine that isn't reporting runs nothing", () => {
    expect(shown(build("?f=runs&runs=node%3Anode-a"))).toEqual({ alpha: ["a1"], gamma: ["g1"] });
    expect(shown(build("?f=runs&runs=node%3Anode-b"))).toEqual({});
  });

  it("shows the whole Catalog once the chip is removed, hiding nothing", () => {
    const table = build("?f=");

    expect(shown(table)).toEqual({ alpha: ["a1", "a2"], beta: ["b1"], gamma: ["g1"] });
    expect(table.rows.every((entry) => entry.hiddenChildCount === 0)).toBe(true);
  });

  it("applies nothing without the Runs on field, keeping the chip in the view", () => {
    const table = build("", false);
    const view: CatalogViewState = catalogViewCodec.parse("");

    expect(view.predicates).toEqual([{ field: "runsOn", values: ["site"] }]);
    expect(shown(table)).toEqual({ alpha: ["a1", "a2"], beta: ["b1"], gamma: ["g1"] });
    expect(table.facets.runsOn).toEqual([]);
  });

  it("keeps a machine in the URL readable before the field is known", () => {
    expect(catalogViewCodec.parse("?f=runs&runs=node%3Anode-a").predicates).toEqual([
      { field: "runsOn", values: ["node:node-a"] },
    ]);
  });

  it("sorts by intelligence, highest first, with the unscored model last", () => {
    const order = (search: string) => build(search).rows.map((entry) => entry.row.modelId);

    expect(order("?f=")).toEqual(["beta", "alpha", "gamma"]);
    expect(order("?f=&sort=intelligence")).toEqual(["alpha", "beta", "gamma"]);
    expect(order("?f=&sort=name")).toEqual(["alpha", "beta", "gamma"]);
  });

  it("orders by name, not a hidden Intelligence column, while a memory sort waits for estimates", () => {
    const order = (search: string, pending: boolean) =>
      build(search, true, pending).rows.map((entry) => entry.row.modelId);

    expect(order("?f=&sort=memory&cols=model%2Cmemory", true)).toEqual(["alpha", "beta", "gamma"]);
    expect(order("?f=&sort=memory", true)).toEqual(["beta", "alpha", "gamma"]);
  });

  it("evaluates a site without model targets as not applicable", () => {
    expect(
      catalogRunsOn(
        indexProfileCompatibility(create(ListModelProfileCompatibilityResponseSchema, {})),
        [],
      ),
    ).toBeUndefined();
  });
});

describe("Catalog Runs on memory fit", () => {
  // On node-a, a1 fits and a2 exceeds; b1 can't be judged; g1 can't serve the scenario. On node-c,
  // a2 and g1 fit and b1 can't run.
  const other = { executionTargetId: "gpu1", siteNodeId: "node-c" };
  function fitRead() {
    const parts = [
      compatibilityResponse(
        "alpha",
        targetCompatibility(working, { a1: {}, a2: { memoryFit: MemoryFit.EXCEEDS } }),
        targetCompatibility(other, { a1: { reason: driverTooOld }, a2: {} }),
      ),
      compatibilityResponse(
        "beta",
        targetCompatibility(working, { b1: { memoryFit: MemoryFit.UNKNOWN } }),
        targetCompatibility(other, { b1: { reason: driverTooOld } }),
      ),
      compatibilityResponse(
        "gamma",
        targetCompatibility(working, {
          g1: {
            memoryFit: MemoryFit.UNSPECIFIED,
            unsupportedReason: SizingScenarioUnsupportedReason.DERIVED_PARAMETER_OUT_OF_RANGE,
          },
        }),
        targetCompatibility(other, { g1: {} }),
      ),
    ];
    return indexProfileCompatibility(
      create(ListModelProfileCompatibilityResponseSchema, {
        entries: parts.flatMap(({ entries }) => entries),
        targets: parts[0].targets,
      }),
    );
  }
  function fitTable(search: string, { fit = true } = {}) {
    return buildCatalogTable({
      estimatesPending: false,
      rows,
      runsOn: catalogRunsOn(fitRead(), [], { fit }),
      view: catalogViewCodec.parse(search),
    });
  }

  it("keeps fits and unknown fit on a machine, hiding exceeds and unsupported, counted by why", () => {
    const table = fitTable("?f=runs&runs=node%3Anode-a");

    expect(shown(table)).toEqual({ alpha: ["a1"], beta: ["b1"] });
    expect(table.hiddenByFit).toEqual({ exceeds: 1, unsupported: 1 });
    expect(table.facets.runsOn).toEqual([
      { count: 3, label: "this site", value: "site" },
      { count: 2, label: "node-a", value: "node:node-a" },
      { count: 2, label: "node-c", value: "node:node-c" },
    ]);
  });

  it("keeps a profile that exceeds on one selected machine but fits on another", () => {
    const table = fitTable("?f=runs&runs=node%3Anode-a&runs=node%3Anode-c");

    expect(shown(table)).toEqual({ alpha: ["a1", "a2"], beta: ["b1"], gamma: ["g1"] });
    expect(table.hiddenByFit).toEqual({ exceeds: 0, unsupported: 0 });
  });

  it("fits on the site when any machine fits", () => {
    expect(shown(fitTable(""))).toEqual({ alpha: ["a1", "a2"], beta: ["b1"], gamma: ["g1"] });
  });

  it("hides a model whose only profile exceeds everywhere it runs, and says none runs", () => {
    const table = buildCatalogTable({
      estimatesPending: false,
      rows: [row("alpha", "Alpha", 40, ["a2"])],
      runsOn: catalogRunsOn(fitRead(), []),
      view: catalogViewCodec.parse("?f=runs&runs=node%3Anode-a"),
    });

    expect(shown(table)).toEqual({});
    expect(table.noneRun).toBe(true);
    expect(table.hiddenByFit).toEqual({ exceeds: 1, unsupported: 0 });
  });

  it("counts only the profiles the other filters keep", () => {
    expect(fitTable("?f=runs&runs=node%3Anode-a&q=gamma").hiddenByFit).toEqual({
      exceeds: 0,
      unsupported: 1,
    });
  });

  it("judges support alone without fit, such as after the scenario's read failed", () => {
    const table = fitTable("?f=runs&runs=node%3Anode-a", { fit: false });

    expect(shown(table)).toEqual({ alpha: ["a1", "a2"], beta: ["b1"], gamma: ["g1"] });
    expect(table.hiddenByFit).toEqual({ exceeds: 0, unsupported: 0 });
  });

  it("hides nothing once the chip is removed", () => {
    const table = fitTable("?f=");

    expect(shown(table)).toEqual({ alpha: ["a1", "a2"], beta: ["b1"], gamma: ["g1"] });
    expect(table.hiddenByFit).toEqual({ exceeds: 0, unsupported: 0 });
  });

  it("locates the catalog on the selected machines, else on the site", () => {
    const runsOn = catalogRunsOn(fitRead(), [])!;

    expect(catalogLocation(runsOn, ["node:node-a", "node:node-c"])).toEqual({
      name: "node-a or node-c",
      scopes: [
        { kind: "node", siteNodeId: "node-a" },
        { kind: "node", siteNodeId: "node-c" },
      ],
    });
    for (const values of [[], ["site"], ["site", "node:node-a"]]) {
      expect(catalogLocation(runsOn, values)).toEqual({
        name: "this site",
        scopes: [{ kind: "site" }],
      });
    }
  });
});

describe("Catalog Runs on memory basis", () => {
  // On node-a, a1 fits both bases, a2 fits the total but not free memory, and b1 fits the total
  // while its free memory is unknown.
  function basisRead() {
    const parts = [
      compatibilityResponse(
        "alpha",
        targetCompatibility(working, {
          a1: {},
          a2: { freeMemoryFit: MemoryFit.EXCEEDS, memoryFit: MemoryFit.FITS },
        }),
      ),
      compatibilityResponse(
        "beta",
        targetCompatibility(working, {
          b1: { freeMemoryFit: MemoryFit.UNKNOWN, memoryFit: MemoryFit.FITS },
        }),
      ),
    ];
    return indexProfileCompatibility(
      create(ListModelProfileCompatibilityResponseSchema, {
        entries: parts.flatMap(({ entries }) => entries),
        targets: parts[0].targets,
      }),
    );
  }
  function basisTable(basis?: "free" | "total") {
    return buildCatalogTable({
      estimatesPending: false,
      rows: rows.slice(0, 2),
      runsOn: catalogRunsOn(basisRead(), [], basis ? { basis } : {}),
      view: catalogViewCodec.parse("?f=runs&runs=node%3Anode-a"),
    });
  }

  it("hides a profile that fits the total but not free memory on free now, the default, and counts it", () => {
    for (const table of [basisTable(), basisTable("free")]) {
      expect(shown(table)).toEqual({ alpha: ["a1"], beta: ["b1"] });
      expect(table.hiddenByFit).toEqual({ exceeds: 1, unsupported: 0 });
    }
  });

  it("shows it on the total, hiding nothing", () => {
    const table = basisTable("total");

    expect(shown(table)).toEqual({ alpha: ["a1", "a2"], beta: ["b1"] });
    expect(table.hiddenByFit).toEqual({ exceeds: 0, unsupported: 0 });
  });
});

function row(
  modelId: string,
  displayName: string,
  referenceScore: number | undefined,
  profileIds: string[],
): ModelRow {
  const profiles = profileIds.map((profileId) => profile(modelId, profileId, referenceScore));
  return {
    colorKey: modelId,
    displayName,
    modelId,
    profileCount: profiles.length,
    profiles,
    referenceScore,
    runningCount: 0,
    runningStatus: undefined,
    speculativeMethods: [],
    requestKinds: [],
    supportsVision: false,
  };
}

function profile(modelId: string, profileId: string, referenceScore?: number): ProfileRow {
  return {
    acceleratorRequirements: [],
    configurationId: "config",
    displayName: profileId,
    engineKinds: ["llama-cpp"],
    hardwareClassEstimates: [],
    scenarioUnsupportedClasses: [],
    imageInput: false,
    modelId,
    platforms: [],
    profileId,
    referenceScore,
    speculativeOptions: [],
  };
}
