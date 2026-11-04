import { clone, create } from "@bufbuild/protobuf";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent, { type UserEvent } from "@testing-library/user-event";
import { type ComponentProps } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  type ExecutionTargetsRead,
  ExecutionTargetsWatch,
  executionTargetsWatch,
} from "@/features/deployments/execution-targets-watch-test-support";
import type { EngineParameterValue } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import { ModelSchema } from "@/gen/dotcore/catalog/v1/model_pb";
import { MemoryFit } from "@/gen/dotcore/deployment/v1/memory_fit_pb";
import { ExactPlacementSchema } from "@/gen/dotcore/deployment/v1/placement_pb";
import {
  ListModelProfileCompatibilityResponseSchema,
  ModelProfileSizingSchema,
  StartingTotalContextSource,
} from "@/gen/dotcore/model/v1/service_pb";
import { AcceleratorMemoryArchitecture } from "@/gen/dotcore/observation/v1/observation_pb";
import {
  ConfiguredExecutionTargetSchema,
  ExecutionTargetWorkloadKind,
  NodeObservationState,
} from "@/gen/dotcore/site/v1/service_pb";

import { ModelDeployDialog } from "./model-deploy-dialog";
import {
  dedicatedPreview,
  devicePool,
  gib,
  other,
  placementPreview,
  previewed,
} from "./model-deploy-memory-test-support";
import {
  compatibilityResponse,
  driverTooOld,
  targetCompatibility,
} from "./profile-compatibility-test-support";
import {
  runtimeSchemaModel,
  runtimeSchemaReleases,
  unsignedValue,
} from "./runtime-schema-test-support";
import type { SizingScenario } from "./sizing-scenario";
import { PROFILE_COMPATIBILITY_REFRESH_INTERVAL_MS } from "./use-profile-compatibility";
import { type ResourceEstimateLoader } from "./use-resource-estimate";

const modelId = runtimeSchemaModel.modelId;
const scenario: SizingScenario = { contextTokensPerRequest: 65536, totalContextTokens: 524288 };
// The Runs on chip's summary of that scenario.
const sizedFor = "512k · 64k per request";
const poolPlacement = { executionTargetId: "pool", siteNodeId: "node" };
const workerPlacement = { executionTargetId: "worker-gpu", siteNodeId: "worker" };

// The engine parameter values the compatibility read returns per profile for the scenario above:
// the profile's defaults with the scenario applied, hidden parameters included.
const scenarioValues = {
  "q4-xl": [
    ["prompt-cache-memory-mib", 8192n],
    ["total-context", 524288n],
    ["parallel-requests", 8n],
    ["context-per-request", 65536n],
  ],
  "nvfp4-text": [
    ["maximum-batched-tokens", 4096n],
    ["maximum-cudagraph-capture-size", 32n],
    ["kv-offloading-size-gib", 0n],
    ["total-context", 524288n],
    ["parallel-requests", 8n],
    ["context-per-request", 65536n],
  ],
  "ninfer-nvfp4": [
    ["parallel-requests", 8n],
    ["host-context-mib", 20480n],
    ["device-state-slots", 4n],
    ["total-context", 524288n],
    ["context-per-request", 65536n],
  ],
} as const satisfies Record<string, readonly (readonly [string, bigint])[]>;

describe("ModelDeployDialog scenario prefill", () => {
  it.each(Object.entries(scenarioValues))(
    "launches %s with the scenario's values, hidden ones included, and no option overrides",
    async (profileId, expected) => {
      const user = userEvent.setup();
      const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
      const onEstimate = vi.fn().mockResolvedValue({});
      const listProfileCompatibility = scenarioLoader({}, [
        targetCompatibility(poolPlacement, { [profileId]: {} }),
      ]);
      renderDialog({
        initialProfileId: profileId,
        listProfileCompatibility,
        onEstimate,
        onSubmit,
        scenario,
      });

      await waitFor(() => expect(onEstimate).toHaveBeenCalled());
      expect(listProfileCompatibility).toHaveBeenCalledWith(modelId, scenario);
      const first = onEstimate.mock.calls[0][0] as EstimateCall;
      expect(first.profileId).toBe(profileId);
      expect(first.placement).toEqual(create(ExactPlacementSchema, poolPlacement));
      expect(pairs(first.configuration.engineParameterValues.values)).toEqual(expected);

      await user.click(await screen.findByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Deploy" }));
      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      const { configuration } = onSubmit.mock.calls[0][0];
      expect(pairs(configuration.engineParameterValues.values)).toEqual(expected);
      expect(configuration.optionOverrides).toEqual([]);
    },
  );

  it("shows why a deploy would be rejected, keeps the estimate and blocks until fixed", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
    const reason = "total context is above what 4 parallel requests use";
    const placementPreview = dedicatedPreview({ accelerator: 8n * gib });
    const onEstimate = vi.fn<ResourceEstimateLoader>(({ configuration }) =>
      Promise.resolve({
        placementPreview,
        launchRejection: configuration.engineParameterValues?.values.some(
          ({ parameterId, value }) =>
            parameterId === "context-per-request" && value.value === 32768n,
        )
          ? reason
          : undefined,
      }),
    );
    renderDialog({
      initialProfileId: "ninfer-nvfp4",
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, { "ninfer-nvfp4": {} }),
      ]),
      onEstimate,
      onSubmit,
      scenario: { totalContextTokens: 524288 },
    });

    await screen.findByRole("radio", { name: /NVFP4 ninfer/ });
    await user.click(screen.getByRole("button", { name: "Next" }));
    const perRequest = screen.getByRole("textbox", { name: /Context per request/ });
    await user.type(perRequest, "32768");
    const message = "Total context is above what 4 parallel requests use.";
    expect(await screen.findByText(message)).toBeVisible();
    expect(screen.queryByText(/Estimate unavailable/)).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Step 2 of 3 — Context")).toBeVisible();
    expect(screen.getByRole("textbox", { name: /Context per request/ })).toHaveFocus();

    await user.clear(perRequest);
    await user.type(perRequest, "65536");
    await waitFor(() => expect(screen.queryByText(message)).not.toBeInTheDocument());
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(await screen.findByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
  });

  it("leaves the context per request empty and unsubmitted when the scenario sets only a total", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
    const totalOnly: SizingScenario = { totalContextTokens: 524288 };
    renderDialog({
      initialProfileId: "ninfer-nvfp4",
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, { "ninfer-nvfp4": {} }),
      ]),
      onSubmit,
      scenario: totalOnly,
    });

    await screen.findByRole("radio", { name: /NVFP4 ninfer/ });
    await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Step 3 of 5 — Context")).toBeVisible();
    expect(screen.getByRole("textbox", { name: /Context per request/ })).toHaveValue("");
    while (!screen.queryByRole("button", { name: "Deploy" }))
      await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(pairs(onSubmit.mock.calls[0][0].configuration.engineParameterValues.values)).toEqual([
      ["parallel-requests", 8n],
      ["host-context-mib", 20480n],
      ["device-state-slots", 4n],
      ["total-context", 524288n],
    ]);
  });

  it("starts the Runs on chip at the catalog's machine", async () => {
    renderDialog({
      initialProfileId: "ninfer-nvfp4",
      catalogChips: [{ field: "runsOn", values: ["node:worker"] }],
      listConfiguredExecutionTargets: () =>
        Promise.resolve({ executionTargets: [poolTarget(), workerTarget()] }),
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, { "ninfer-nvfp4": {} }),
        targetCompatibility(workerPlacement, { "ninfer-nvfp4": {} }),
      ]),
      scenario,
    });

    expect(await screen.findByRole("button", { name: /^Runs on filter: .*worker/i })).toBeVisible();
  });

  it("shows the total context in the Runs on scope: the largest fitting at defaults, else the scenario's", async () => {
    const targets = [
      targetCompatibility(poolPlacement, {
        "ninfer-nvfp4": { startingTotalContextTokens: 98304 },
      }),
      targetCompatibility(workerPlacement, {
        "ninfer-nvfp4": { startingTotalContextTokens: 65536 },
      }),
    ];
    const totalContext = () =>
      within(screen.getByRole("row", { name: "NVFP4 ninfer" }))
        .getAllByRole("cell")
        .at(-1)!.textContent;
    const props = {
      initialProfileId: "ninfer-nvfp4",
      catalogChips: [{ field: "runsOn" as const, values: ["node:worker"] }],
      listConfiguredExecutionTargets: () =>
        Promise.resolve({ executionTargets: [poolTarget(), workerTarget()] }),
      listProfileCompatibility: scenarioLoader({}, targets),
    };
    // At defaults, the worker's largest fitting total rather than the pool's larger one.
    const { unmount } = renderDialog(props);
    await screen.findByRole("button", { name: /^Runs on filter: .*worker/i });
    await waitFor(() => expect(totalContext()).toBe("64k"));
    unmount();

    renderDialog({ ...props, scenario });
    await screen.findByRole("button", { name: /^Runs on filter: .*worker/i });
    await waitFor(() => expect(totalContext()).toBe("512k"));
  });

  it("starts the Runs on chip at every machine the catalog selects", async () => {
    renderDialog({
      initialProfileId: "ninfer-nvfp4",
      catalogChips: [{ field: "runsOn", values: ["node:node", "node:worker"] }],
      listConfiguredExecutionTargets: () =>
        Promise.resolve({ executionTargets: [poolTarget(), workerTarget()] }),
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, { "ninfer-nvfp4": {} }),
        targetCompatibility(workerPlacement, { "ninfer-nvfp4": {} }),
      ]),
      scenario,
    });

    expect(
      await screen.findByRole("button", { name: `Runs on filter: node, Worker, ${sizedFor}` }),
    ).toBeVisible();
  });

  it("hides a profile that exceeds memory behind Runs on, and keeps its advisory once picked", async () => {
    const user = userEvent.setup();
    const onEstimate = vi.fn().mockResolvedValue({});
    renderDialog({
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, {
          "ninfer-nvfp4": { memoryFit: MemoryFit.EXCEEDS },
          "nvfp4-text": {},
        }),
      ]),
      onEstimate,
      scenario,
    });

    expect(
      await screen.findByRole("button", { name: `Runs on filter: this site, ${sizedFor}` }),
    ).toBeVisible();
    await waitFor(() => expect(profilesFooter()).toBe("1 of 4 profiles · 1 hidden"));
    expect(profileRows()).toEqual(["NVFP4 text"]);
    await waitFor(() => expect(screen.getByRole("radio", { name: "NVFP4 text" })).toBeChecked());

    await user.click(screen.getByRole("button", { name: "Remove Runs on filter" }));
    const exceeding = screen.getByRole("row", { name: "NVFP4 ninfer" });
    expect(within(exceeding).getByText("Exceeds free memory on node")).toBeVisible();
    await user.click(within(exceeding).getByRole("radio"));
    expect(within(exceeding).getByRole("radio")).toBeChecked();
    await waitFor(() =>
      expect((onEstimate.mock.lastCall![0] as EstimateCall).profileId).toBe("ninfer-nvfp4"),
    );
    expect(screen.getAllByText(/^Exceeds free memory on /)).toHaveLength(1);
  });

  it("says no profile runs when every running one exceeds", async () => {
    renderDialog({
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, {
          "ninfer-nvfp4": { memoryFit: MemoryFit.EXCEEDS },
          "nvfp4-text": { memoryFit: MemoryFit.EXCEEDS },
        }),
      ]),
      scenario,
    });

    expect(await screen.findByText("No profile runs here")).toBeVisible();
    expect(profilesFooter()).toBe("0 of 4 profiles · 2 hidden");
  });

  it("keeps the shown verdicts while a new scenario's read is pending, and drops the fit once it fails", async () => {
    const reads: { reject: (error: Error) => void }[] = [];
    const fitRead = scenarioLoader({}, [
      targetCompatibility(poolPlacement, {
        "ninfer-nvfp4": { memoryFit: MemoryFit.EXCEEDS },
        "nvfp4-text": {},
      }),
    ]);
    const listProfileCompatibility = vi.fn((id: string, requested?: SizingScenario) =>
      requested === scenario
        ? fitRead(id, requested)
        : new Promise<never>((_resolve, reject) => reads.push({ reject })),
    );
    const user = userEvent.setup();
    renderDialog({ listProfileCompatibility, scenario });
    await waitFor(() => expect(profilesFooter()).toBe("1 of 4 profiles · 1 hidden"));

    await chooseSizedFor(user, `Runs on filter: this site, ${sizedFor}`, "Defaults");
    await waitFor(() => expect(reads).toHaveLength(1));
    // Still hidden by the previous read.
    expect(profileRows()).toEqual(["NVFP4 text"]);
    expect(profilesFooter()).toBe("1 of 4 profiles · 1 hidden");

    act(() => reads[0].reject(new Error("read failed")));
    await waitFor(() => expect(profileRows()).toEqual(["NVFP4 ninfer", "NVFP4 text"]));
    expect(profilesFooter()).toBe("2 of 4 profiles");
  });

  it("keeps a requested profile that exceeds memory visible and selected, without the Runs on chip", async () => {
    renderDialog({
      initialProfileId: "ninfer-nvfp4",
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, {
          "ninfer-nvfp4": { memoryFit: MemoryFit.EXCEEDS },
          "nvfp4-text": {},
        }),
      ]),
      scenario,
    });

    const exceeding = await screen.findByRole("row", { name: "NVFP4 ninfer" });
    await waitFor(() =>
      expect(within(exceeding).getByText("Exceeds free memory on node")).toBeVisible(),
    );
    expect(within(exceeding).getByRole("radio")).toBeChecked();
    expect(screen.queryByRole("button", { name: /^Runs on filter/ })).not.toBeInTheDocument();
  });

  it("drops the catalog's chips that would hide a requested profile", async () => {
    renderDialog({
      catalogChips: [
        { field: "runsOn", values: ["site"] },
        { field: "engine", values: ["vllm"] },
      ],
      initialProfileId: "ninfer-nvfp4",
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, { "ninfer-nvfp4": {}, "nvfp4-text": {} }),
      ]),
      scenario,
    });

    expect(
      await screen.findByRole("button", { name: `Runs on filter: this site, ${sizedFor}` }),
    ).toBeVisible();
    expect(screen.queryByRole("button", { name: /^Engine filter/ })).not.toBeInTheDocument();
    expect(screen.getByRole("radio", { name: "NVFP4 ninfer" })).toBeChecked();
  });

  it("starts without the Runs on chip when the catalog has none, listing every profile", async () => {
    const onEstimate = vi.fn().mockResolvedValue({});
    renderDialog({
      catalogChips: [],
      listConfiguredExecutionTargets: () =>
        Promise.resolve({ executionTargets: [poolTarget(), workerTarget()] }),
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, { "ninfer-nvfp4": {} }),
        targetCompatibility(workerPlacement, {
          "ninfer-nvfp4": { reason: driverTooOld },
          "nvfp4-text": {},
        }),
      ]),
      onEstimate,
      scenario,
    });

    await waitFor(() => expect(onEstimate).toHaveBeenCalled());
    expect(screen.queryByRole("button", { name: /^Runs on filter/ })).not.toBeInTheDocument();
    expect(profileRows()).toEqual(["NVFP4 ninfer", "NVFP4 text", "Q4_K_XL", "Q4_K_XL without MTP"]);
  });

  it("picks the initial profile from the profiles the catalog's chips show when the read fails", async () => {
    const onEstimate = vi.fn().mockResolvedValue({});
    renderDialog({
      catalogChips: [{ field: "engine", values: ["vllm"] }],
      listProfileCompatibility: vi.fn(() => Promise.reject(new Error("read failed"))),
      onEstimate,
      scenario,
    });

    expect(
      await screen.findByText("Couldn't check which profiles can run on this site."),
    ).toBeVisible();
    expect(profileRows()).toEqual(["NVFP4 text"]);
    expect(screen.getByRole("radio", { name: "NVFP4 text" })).toBeChecked();
    await waitFor(() => expect(onEstimate).toHaveBeenCalled());
    expect((onEstimate.mock.lastCall![0] as EstimateCall).profileId).toBe("nvfp4-text");
  });

  it("moves a profile picked before the read when the arriving Runs on chip hides it", async () => {
    const user = userEvent.setup();
    let release = () => undefined as void;
    const held = new Promise<void>((resolve) => {
      release = resolve;
    });
    const fitRead = scenarioLoader({}, [
      targetCompatibility(poolPlacement, {
        "ninfer-nvfp4": {},
        "nvfp4-text": { memoryFit: MemoryFit.EXCEEDS },
      }),
    ]);
    const listProfileCompatibility = vi.fn(async (id: string, requested?: SizingScenario) => {
      await held;
      return fitRead(id, requested);
    });
    renderDialog({ listProfileCompatibility, scenario });

    // NVFP4 ninfer starts selected; the operator picks NVFP4 text instead.
    await user.click(await screen.findByRole("radio", { name: "NVFP4 text" }));
    expect(screen.getByRole("radio", { name: "NVFP4 text" })).toBeChecked();

    await act(async () => {
      release();
      await held;
    });
    expect(
      await screen.findByRole("button", { name: `Runs on filter: this site, ${sizedFor}` }),
    ).toBeVisible();
    expect(profileRows()).toEqual(["NVFP4 ninfer"]);
    await waitFor(() => expect(screen.getByRole("radio", { name: "NVFP4 ninfer" })).toBeChecked());
  });

  it("opens on the second running target when the first exceeds memory and the second fits", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
    const onEstimate = vi.fn().mockResolvedValue({});
    renderDialog({
      initialProfileId: "ninfer-nvfp4",
      listConfiguredExecutionTargets: () =>
        Promise.resolve({ executionTargets: [poolTarget(), workerTarget()] }),
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, {
          "ninfer-nvfp4": { memoryFit: MemoryFit.EXCEEDS },
        }),
        targetCompatibility(workerPlacement, {
          "ninfer-nvfp4": { memoryFit: MemoryFit.FITS },
        }),
      ]),
      onEstimate,
      onSubmit,
      scenario,
    });

    await waitFor(() => expect(onEstimate).toHaveBeenCalled());
    expect((onEstimate.mock.calls[0][0] as EstimateCall).placement).toEqual(
      create(ExactPlacementSchema, workerPlacement),
    );

    await user.click(await screen.findByRole("button", { name: "Next" }));
    await user.click(await screen.findByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(onSubmit.mock.calls[0][0].placement).toEqual(
      create(ExactPlacementSchema, workerPlacement),
    );
  });

  it("hides a profile that fits the total but not free memory on free now, and shows it on Total", async () => {
    const user = userEvent.setup();
    renderDialog({
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, {
          "ninfer-nvfp4": { freeMemoryFit: MemoryFit.EXCEEDS, memoryFit: MemoryFit.FITS },
          "nvfp4-text": {},
        }),
      ]),
      scenario,
    });

    const chip = `Runs on filter: this site, ${sizedFor}`;
    await waitFor(() => expect(profilesFooter()).toBe("1 of 4 profiles · 1 hidden"));
    expect(profileRows()).toEqual(["NVFP4 text"]);

    await user.click(await screen.findByRole("button", { name: chip }));
    const menu = await screen.findByRole("menu");
    expect(within(menu).getByRole("menuitemradio", { name: "Free now" })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await user.click(within(menu).getByRole("menuitemradio", { name: "Total" }));
    await user.keyboard("{Escape}");
    const totalChip = `Runs on filter: this site, ${sizedFor} · Total memory`;
    expect(await screen.findByRole("button", { name: totalChip })).toBeVisible();
    await waitFor(() => expect(profilesFooter()).toBe("2 of 4 profiles"));

    // Reset returns to free now.
    await user.click(screen.getByRole("button", { name: "Reset" }));
    expect(await screen.findByRole("button", { name: chip })).toBeVisible();
    await waitFor(() => expect(profilesFooter()).toBe("1 of 4 profiles · 1 hidden"));
  });

  it("starts on the opener's Total, preferring a target that fits the total", async () => {
    const onEstimate = vi.fn().mockResolvedValue({});
    renderDialog({
      initialProfileId: "ninfer-nvfp4",
      listConfiguredExecutionTargets: () =>
        Promise.resolve({ executionTargets: [poolTarget(), workerTarget()] }),
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, {
          "ninfer-nvfp4": { freeMemoryFit: MemoryFit.EXCEEDS, memoryFit: MemoryFit.FITS },
        }),
        targetCompatibility(workerPlacement, {
          "ninfer-nvfp4": { freeMemoryFit: MemoryFit.FITS, memoryFit: MemoryFit.EXCEEDS },
        }),
      ]),
      memoryBasis: "total",
      onEstimate,
      scenario,
    });

    expect(
      await screen.findByRole("button", {
        name: `Runs on filter: this site, ${sizedFor} · Total memory`,
      }),
    ).toBeVisible();
    await waitFor(() => expect(onEstimate).toHaveBeenCalled());
    expect((onEstimate.mock.calls[0][0] as EstimateCall).placement).toEqual(
      create(ExactPlacementSchema, poolPlacement),
    );
  });

  it("prefers a target with enough free memory on free now", async () => {
    const onEstimate = vi.fn().mockResolvedValue({});
    renderDialog({
      initialProfileId: "ninfer-nvfp4",
      listConfiguredExecutionTargets: () =>
        Promise.resolve({ executionTargets: [poolTarget(), workerTarget()] }),
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, {
          "ninfer-nvfp4": { freeMemoryFit: MemoryFit.EXCEEDS, memoryFit: MemoryFit.FITS },
        }),
        targetCompatibility(workerPlacement, {
          "ninfer-nvfp4": { freeMemoryFit: MemoryFit.FITS, memoryFit: MemoryFit.FITS },
        }),
      ]),
      onEstimate,
      scenario,
    });

    await waitFor(() => expect(onEstimate).toHaveBeenCalled());
    expect((onEstimate.mock.calls[0][0] as EstimateCall).placement).toEqual(
      create(ExactPlacementSchema, workerPlacement),
    );
  });

  it('re-sizes the launch from a "Sized for" changed in the dialog', async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
    const listProfileCompatibility = scenarioLoader({}, [
      targetCompatibility(poolPlacement, { "ninfer-nvfp4": {} }),
    ]);
    renderDialog({
      initialProfileId: "ninfer-nvfp4",
      listProfileCompatibility,
      onSubmit,
      scenario,
    });

    await chooseSizedFor(user, `Runs on filter: this site, ${sizedFor}`, "Defaults");
    expect(await screen.findByRole("button", { name: "Runs on filter: this site" })).toBeVisible();
    await waitFor(() => expect(listProfileCompatibility).toHaveBeenLastCalledWith(modelId));
    await user.click(await screen.findByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(pairs(onSubmit.mock.calls[0][0].configuration.engineParameterValues.values)).toEqual([
      ["parallel-requests", 4n],
      ["host-context-mib", 20480n],
      ["device-state-slots", 4n],
      ["total-context", 32768n],
    ]);
  });

  it("opens a profile with an engine-owned total context at the read's value and submits it", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
    const listProfileCompatibility = vi.fn(() =>
      Promise.resolve(
        scenarioResponse([targetCompatibility(poolPlacement, { "q4-xl": {} })], {
          "q4-xl": [
            ["prompt-cache-memory-mib", 8192n],
            ["total-context", 16384n],
            ["parallel-requests", 1n],
          ],
        }),
      ),
    );
    renderDialog({ initialProfileId: "q4-xl", listProfileCompatibility, onSubmit });

    await user.click(await screen.findByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(listProfileCompatibility).toHaveBeenCalledWith(modelId);
    expect(pairs(onSubmit.mock.calls[0][0].configuration.engineParameterValues.values)).toEqual([
      ["prompt-cache-memory-mib", 8192n],
      ["total-context", 16384n],
      ["parallel-requests", 1n],
    ]);
  });

  it("starts the other values from the read without a scenario when the scenario's read has none for the profile", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
    const targets = [targetCompatibility(poolPlacement, { "q4-xl": {} })];
    const listProfileCompatibility = vi.fn((_id: string, requested?: SizingScenario) =>
      Promise.resolve(
        scenarioResponse(
          targets,
          requested
            ? {}
            : {
                "q4-xl": [
                  ["prompt-cache-memory-mib", 8192n],
                  ["total-context", 16384n],
                  ["parallel-requests", 1n],
                ],
              },
        ),
      ),
    );
    renderDialog({ initialProfileId: "q4-xl", listProfileCompatibility, onSubmit, scenario });

    await user.click(await screen.findByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(listProfileCompatibility).toHaveBeenCalledWith(modelId, scenario);
    expect(listProfileCompatibility).toHaveBeenCalledWith(modelId);
    expect(pairs(onSubmit.mock.calls[0][0].configuration.engineParameterValues.values)).toEqual([
      ["prompt-cache-memory-mib", 8192n],
      ["total-context", 524288n],
      ["parallel-requests", 1n],
      ["context-per-request", 65536n],
    ]);
  });

  it("keeps the release default for a profile whose read carries no values", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
    renderDialog({
      initialProfileId: "q4-xl",
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, { "q4-xl": {} }),
      ]),
      onSubmit,
    });

    await user.click(await screen.findByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    const submitted = pairs(onSubmit.mock.calls[0][0].configuration.engineParameterValues.values);
    expect(submitted.find(([id]) => id === "total-context")).toEqual(["total-context", 32768n]);
  });

  it('shows Reset once "Sized for" leaves the opener\'s, and restores it', async () => {
    const user = userEvent.setup();
    const listProfileCompatibility = scenarioLoader({}, [
      targetCompatibility(poolPlacement, { "ninfer-nvfp4": {} }),
    ]);
    renderDialog({ initialProfileId: "ninfer-nvfp4", listProfileCompatibility, scenario });
    const chip = `Runs on filter: this site, ${sizedFor}`;
    expect(await screen.findByRole("button", { name: chip })).toBeVisible();
    expect(screen.queryByRole("button", { name: "Reset" })).not.toBeInTheDocument();

    await chooseSizedFor(user, chip, "Defaults");
    await user.click(await screen.findByRole("button", { name: "Reset" }));

    expect(await screen.findByRole("button", { name: chip })).toBeVisible();
    expect(screen.queryByRole("button", { name: "Reset" })).not.toBeInTheDocument();
    await waitFor(() =>
      expect(listProfileCompatibility).toHaveBeenLastCalledWith(modelId, scenario),
    );
  });

  it('drops "Sized for" with the Runs on chip, and Reset restores both', async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
    const listProfileCompatibility = scenarioLoader({}, [
      targetCompatibility(poolPlacement, { "ninfer-nvfp4": {} }),
    ]);
    renderDialog({
      initialProfileId: "ninfer-nvfp4",
      listProfileCompatibility,
      onSubmit,
      scenario,
    });
    const chip = `Runs on filter: this site, ${sizedFor}`;
    expect(await screen.findByRole("button", { name: chip })).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Remove Runs on filter" }));
    await waitFor(() => expect(listProfileCompatibility).toHaveBeenLastCalledWith(modelId));

    // Added back, the chip starts without a "Sized for".
    await user.click(screen.getByRole("button", { name: "Filter" }));
    await user.click(await screen.findByRole("menuitem", { name: "Runs on" }));
    await user.keyboard("{Escape}");
    expect(await screen.findByRole("button", { name: /^Runs on filter/ })).not.toHaveAccessibleName(
      new RegExp(sizedFor),
    );
    await user.click(screen.getByRole("button", { name: "Remove Runs on filter" }));

    await user.click(await screen.findByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(pairs(onSubmit.mock.calls[0][0].configuration.engineParameterValues.values)).toEqual([
      ["parallel-requests", 4n],
      ["host-context-mib", 20480n],
      ["device-state-slots", 4n],
      ["total-context", 32768n],
    ]);
  });

  it('restores the chip and its "Sized for" in one Reset after the chip is removed', async () => {
    const user = userEvent.setup();
    const listProfileCompatibility = scenarioLoader({}, [
      targetCompatibility(poolPlacement, { "ninfer-nvfp4": {} }),
    ]);
    renderDialog({ initialProfileId: "ninfer-nvfp4", listProfileCompatibility, scenario });
    const chip = `Runs on filter: this site, ${sizedFor}`;
    expect(await screen.findByRole("button", { name: chip })).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Remove Runs on filter" }));
    await user.click(await screen.findByRole("button", { name: "Reset" }));

    expect(await screen.findByRole("button", { name: chip })).toBeVisible();
    expect(screen.queryByRole("button", { name: "Reset" })).not.toBeInTheDocument();
    await waitFor(() =>
      expect(listProfileCompatibility).toHaveBeenLastCalledWith(modelId, scenario),
    );
  });

  it("launches the profile defaults without a scenario", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
    const listProfileCompatibility = scenarioLoader({}, [
      targetCompatibility(poolPlacement, { "ninfer-nvfp4": {} }),
    ]);
    renderDialog({ initialProfileId: "ninfer-nvfp4", listProfileCompatibility, onSubmit });

    await user.click(await screen.findByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(listProfileCompatibility).toHaveBeenCalledWith(modelId);
    expect(pairs(onSubmit.mock.calls[0][0].configuration.engineParameterValues.values)).toEqual([
      ["parallel-requests", 4n],
      ["host-context-mib", 20480n],
      ["device-state-slots", 4n],
      ["total-context", 32768n],
    ]);
  });
});

describe("ModelDeployDialog largest fitting total context", () => {
  it("shows the selected target's largest fitting total context and follows a target change", async () => {
    const user = userEvent.setup();
    renderDialog({
      initialProfileId: "ninfer-nvfp4",
      listConfiguredExecutionTargets: () =>
        Promise.resolve({ executionTargets: [poolTarget(), workerTarget()] }),
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, {
          "ninfer-nvfp4": { startingTotalContextTokens: 126976 },
        }),
        targetCompatibility(workerPlacement, {
          "ninfer-nvfp4": { startingTotalContextTokens: 409600 },
        }),
      ]),
    });

    await screen.findByRole("radio", { name: /NVFP4 ninfer/ });
    await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
    await pickTarget(user, /^node \/ NVIDIA GeForce RTX 5090/);
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Step 3 of 5 — Context")).toBeVisible();
    const hint = "Up to 124k on node / NVIDIA GeForce RTX 5090";
    expect(screen.getByText(hint)).toBeVisible();
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveAccessibleDescription(
      new RegExp(`^${hint} `),
    );

    await user.click(screen.getByRole("button", { name: /Target/ }));
    await pickTarget(user, /^Worker \/ NVIDIA GB10/);
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Up to 400k on Worker / NVIDIA GB10")).toBeVisible();
    expect(screen.queryByText(hint)).not.toBeInTheDocument();
  });
});

describe("ModelDeployDialog largest fit under a new scenario", () => {
  it('picks up the fit of a new "Sized for" from its own read', async () => {
    const user = userEvent.setup();
    renderDialog({
      initialProfileId: "ninfer-nvfp4",
      listProfileCompatibility: vi.fn((_modelId: string, requested?: SizingScenario) =>
        Promise.resolve(
          scenarioResponse(
            [
              targetCompatibility(poolPlacement, {
                "ninfer-nvfp4": requested
                  ? { startingTotalContextTokens: 126976 }
                  : { startingTotalContextTokens: 98304 },
              }),
            ],
            requested ? scenarioValues : undefined,
          ),
        ),
      ),
      scenario,
    });

    const chip = `Runs on filter: this site, ${sizedFor}`;
    await screen.findByRole("button", { name: chip });
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(await screen.findByText(/^Up to 124k on /)).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Back" }));
    await chooseSizedFor(user, chip, "Defaults");
    await user.click(screen.getByRole("button", { name: "Next" }));

    expect(await screen.findByText(/^Up to 96k on /)).toBeVisible();
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("98304");
  });
});

describe("ModelDeployDialog shared context", () => {
  it("starts total context at the largest fit rounded down to 1024 tokens, as the table shows", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
    renderDialog({
      initialProfileId: "q4-xl",
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, {
          "q4-xl": { startingTotalContextTokens: 910336 },
        }),
      ]),
      model: longContextModel,
      onSubmit,
    });

    await screen.findByRole("button", { name: "Runs on filter: this site" });
    await waitFor(() => expect(totalContextCell("Q4_K_XL")).toBe("889k"));
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("910336");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(summaryContext()).toBe("889k total · 256k per request");
    await user.click(screen.getByRole("button", { name: "Deploy" }));

    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(pairs(onSubmit.mock.calls[0][0].configuration.engineParameterValues.values)).toEqual([
      ["prompt-cache-memory-mib", 8192n],
      ["total-context", 910336n],
      ["parallel-requests", 4n],
    ]);
  });

  it.each([
    {
      case: "a fit above parallel requests × the maximum",
      hint: "Up to 1953k on node / NVIDIA GeForce RTX 5090",
      source: StartingTotalContextSource.LARGEST_FIT,
      starting: 1_999_872,
      total: 1999872n,
    },
    {
      case: "no fit",
      hint: undefined,
      source: StartingTotalContextSource.PROFILE_DEFAULT,
      starting: 32768,
      total: 32768n,
    },
  ])("starts total context at $total with $case", async ({ hint, source, starting, total }) => {
    const user = userEvent.setup();
    const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
    renderDialog({
      initialProfileId: "q4-xl",
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, {
          "q4-xl": {
            startingTotalContextSource: source,
            startingTotalContextTokens: starting,
          },
        }),
      ]),
      model: longContextModel,
      onSubmit,
    });

    await screen.findByRole("button", { name: "Runs on filter: this site" });
    await user.click(screen.getByRole("button", { name: "Next" }));
    // Only a starting value from the largest fit is hinted and labeled as one.
    expect(screen.queryByText(/^Up to .* on /)?.textContent).toBe(hint);
    expect(screen.queryByText("Largest fit") !== null).toBe(hint !== undefined);
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));

    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    const submitted = pairs(onSubmit.mock.calls[0][0].configuration.engineParameterValues.values);
    expect(submitted.find(([id]) => id === "total-context")).toEqual(["total-context", total]);
  });

  it("starts total context at the engine default when the read fails", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
    renderDialog({
      initialProfileId: "q4-xl",
      listProfileCompatibility: vi.fn(() => Promise.reject(new Error("read failed"))),
      model: longContextModel,
      onSubmit,
    });

    await screen.findByText("Couldn't check which profiles can run on this site.");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("32768");
  });

  it("keeps the default total context when parallel requests change on the Runtime step", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
    renderDialog({
      initialProfileId: "q4-xl",
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, {
          "q4-xl": { startingTotalContextTokens: 910336 },
        }),
      ]),
      model: longContextModel,
      onSubmit,
    });

    await screen.findByRole("button", { name: "Runs on filter: this site" });
    await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Step 3 of 5 — Context")).toBeVisible();
    await waitFor(() =>
      expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("910336"),
    );
    expect(screen.queryByRole("textbox", { name: "Parallel requests" })).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Next" }));
    const parallel = screen.getByRole("textbox", { name: "Parallel requests" });
    expect(parallel).toHaveValue("4");
    await user.clear(parallel);
    await user.type(parallel, "1");
    await user.click(screen.getByRole("button", { name: "Back" }));
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("910336");
    expect(screen.getByText("Largest fit")).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(pairs(onSubmit.mock.calls[0][0].configuration.engineParameterValues.values)).toEqual([
      ["prompt-cache-memory-mib", 8192n],
      ["total-context", 910336n],
      ["parallel-requests", 1n],
    ]);
  });

  it("labels where the total context comes from, and returns an edited one to the largest fit", async () => {
    const user = userEvent.setup();
    renderDialog({
      initialProfileId: "q4-xl",
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, {
          "q4-xl": { startingTotalContextTokens: 910336 },
        }),
      ]),
      model: longContextModel,
    });

    await screen.findByRole("button", { name: "Runs on filter: this site" });
    await user.click(screen.getByRole("button", { name: "Next" }));
    const total = screen.getByRole("textbox", { name: "Total context" });
    expect(await screen.findByText("Largest fit")).toBeVisible();
    expect(screen.queryByRole("button", { name: "Use largest fit" })).not.toBeInTheDocument();

    await user.click(
      within(screen.getByRole("group", { name: "Total context presets" })).getByRole("button", {
        name: "64k",
      }),
    );
    expect(total).toHaveValue("65536");
    expect(screen.getByText("Custom")).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Back" }));
    expect(screen.getByRole("button", { name: "Runs on filter: this site, 64k" })).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Use largest fit" }));
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("910336");
    expect(screen.getByText("Largest fit")).toBeVisible();
    expect(screen.getByText("Up to 889k on node / NVIDIA GeForce RTX 5090")).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Back" }));
    expect(screen.getByRole("button", { name: "Runs on filter: this site" })).toBeVisible();
  });

  it("labels the total context custom only when its value leaves the largest fit", async () => {
    const user = userEvent.setup();
    renderDialog({
      initialProfileId: "q4-xl",
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, {
          "q4-xl": { startingTotalContextTokens: 910336 },
        }),
      ]),
      model: longContextModel,
    });

    await screen.findByRole("button", { name: "Runs on filter: this site" });
    await user.click(screen.getByRole("button", { name: "Next" }));
    const total = screen.getByRole("textbox", { name: "Total context" });
    const perRequest = screen.getByRole("textbox", { name: "Context per request" });
    expect(await screen.findByText("Largest fit")).toBeVisible();

    // Per request only: the total keeps the largest fit's value and badge.
    await user.clear(perRequest);
    await user.type(perRequest, "32768");
    await user.tab();
    expect(total).toHaveValue("910336");
    expect(screen.getByText("Largest fit")).toBeVisible();
    expect(screen.queryByText("Custom")).not.toBeInTheDocument();

    await user.clear(total);
    await user.type(total, "65536");
    await user.tab();
    expect(screen.getByText("Custom")).toBeVisible();
    expect(screen.getByRole("button", { name: "Use largest fit" })).toBeVisible();

    await user.clear(total);
    await user.type(total, "910336");
    await user.tab();
    expect(screen.getByText("Largest fit")).toBeVisible();
    expect(screen.queryByText("Custom")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Use largest fit" })).not.toBeInTheDocument();
  });

  it('drops a stale total context error when "Sized for" chooses the context', async () => {
    const user = userEvent.setup();
    renderDialog({
      initialProfileId: "q4-xl",
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, {
          "q4-xl": { startingTotalContextTokens: 910336 },
        }),
      ]),
      model: longContextModel,
    });

    await screen.findByRole("button", { name: "Runs on filter: this site" });
    await user.click(screen.getByRole("button", { name: "Next" }));
    const total = screen.getByRole("textbox", { name: "Total context" });
    await user.clear(total);
    await user.type(total, "12");
    await user.tab();
    expect(await screen.findByText(totalContextError)).toBeVisible();

    await user.click(screen.getByRole("button", { name: /Profile/ }));
    await chooseSizedFor(user, "Runs on filter: this site", "Custom");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("131072");
    expect(screen.queryByText(totalContextError)).not.toBeInTheDocument();
  });

  it("keeps the operator's parallel requests when using the largest fit", async () => {
    const user = userEvent.setup();
    renderDialog({
      initialProfileId: "q4-xl",
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, {
          "q4-xl": { startingTotalContextTokens: 910336 },
        }),
      ]),
      model: longContextModel,
    });

    await screen.findByRole("button", { name: "Runs on filter: this site" });
    await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(
      within(screen.getByRole("group", { name: "Total context presets" })).getByRole("button", {
        name: "64k",
      }),
    );
    await user.click(screen.getByRole("button", { name: "Next" }));
    const parallel = screen.getByRole("textbox", { name: "Parallel requests" });
    await user.clear(parallel);
    await user.type(parallel, "0");
    await user.tab();
    const parallelError = "Parallel requests must be an integer from 1 to 64 requests.";
    expect(await screen.findByText(parallelError)).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Back" }));
    await user.click(screen.getByRole("button", { name: "Use largest fit" }));
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("910336");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByRole("textbox", { name: "Parallel requests" })).toHaveValue("0");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(await screen.findByText(parallelError)).toBeVisible();
    expect(screen.getByText("Step 4 of 5 — Runtime")).toBeVisible();
  });

  it('keeps the operator\'s parallel requests and their error when "Sized for" returns to defaults', async () => {
    const user = userEvent.setup();
    renderDialog({
      initialProfileId: "q4-xl",
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, {
          "q4-xl": { startingTotalContextTokens: 910336 },
        }),
      ]),
      model: longContextModel,
    });

    await screen.findByRole("button", { name: "Runs on filter: this site" });
    await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(
      within(screen.getByRole("group", { name: "Total context presets" })).getByRole("button", {
        name: "64k",
      }),
    );
    await user.click(screen.getByRole("button", { name: "Next" }));
    const parallel = screen.getByRole("textbox", { name: "Parallel requests" });
    await user.clear(parallel);
    await user.type(parallel, "0");
    await user.tab();
    const parallelError = "Parallel requests must be an integer from 1 to 64 requests.";
    expect(await screen.findByText(parallelError)).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Back" }));
    await user.click(screen.getByRole("button", { name: "Back" }));
    await user.click(screen.getByRole("button", { name: "Back" }));
    await chooseSizedFor(user, "Runs on filter: this site, 64k", "Defaults");
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("910336");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByRole("textbox", { name: "Parallel requests" })).toHaveValue("0");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(await screen.findByText(parallelError)).toBeVisible();
    expect(screen.getByText("Step 4 of 5 — Runtime")).toBeVisible();
  });

  it('shares the context between the Context step and "Sized for", with the fit verdicts following', async () => {
    const user = userEvent.setup();
    const listProfileCompatibility = vi.fn((_id: string, requested?: SizingScenario) =>
      Promise.resolve(
        scenarioResponse([
          targetCompatibility(poolPlacement, {
            "q4-xl": { startingTotalContextTokens: 910336 },
            "nvfp4-text": requested ? { memoryFit: MemoryFit.EXCEEDS } : {},
          }),
        ]),
      ),
    );
    renderDialog({ initialProfileId: "q4-xl", listProfileCompatibility, model: longContextModel });

    await screen.findByRole("button", { name: "Runs on filter: this site" });
    await waitFor(() => expect(profileRows()).toEqual(["NVFP4 text", "Q4_K_XL"]));
    await user.click(screen.getByRole("button", { name: "Next" }));
    const total = screen.getByRole("textbox", { name: "Total context" });
    await user.clear(total);
    await user.type(total, "65536");
    await user.tab();
    await user.click(screen.getByRole("button", { name: "Back" }));

    expect(
      await screen.findByRole("button", { name: "Runs on filter: this site, 64k" }),
    ).toBeVisible();
    await waitFor(() =>
      expect(listProfileCompatibility).toHaveBeenCalledWith(modelId, {
        totalContextTokens: 65536,
      }),
    );
    await waitFor(() => expect(profileRows()).toEqual(["Q4_K_XL"]));
    expect(totalContextCell("Q4_K_XL")).toBe("64k");

    // "Sized for" back to defaults returns the Context step to its default.
    await chooseSizedFor(user, "Runs on filter: this site, 64k", "Defaults");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("910336");

    // A custom "Sized for" shows on the Context step.
    await user.click(screen.getByRole("button", { name: "Back" }));
    await chooseSizedFor(user, "Runs on filter: this site", "Custom");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("131072");
  });

  it("keeps an edited total context across profile and target changes", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
    renderDialog({
      initialProfileId: "q4-xl",
      listConfiguredExecutionTargets: () =>
        Promise.resolve({ executionTargets: [poolTarget(), workerTarget()] }),
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, {
          "q4-xl": { startingTotalContextTokens: 910336 },
          "nvfp4-text": { startingTotalContextTokens: 910336 },
        }),
        targetCompatibility(workerPlacement, {
          "q4-xl": { startingTotalContextTokens: 409600 },
          "nvfp4-text": { startingTotalContextTokens: 409600 },
        }),
      ]),
      model: longContextModel,
      onSubmit,
    });

    await screen.findByText("Step 1 of 4 — Profile");
    await pickTarget(user, /^node/);
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Step 3 of 4 — Context")).toBeVisible();
    expect(screen.getByText("Up to 889k on node / NVIDIA GeForce RTX 5090")).toBeVisible();
    const total = screen.getByRole("textbox", { name: "Total context" });
    await user.clear(total);
    await user.type(total, "65536");
    await user.tab();

    await user.click(screen.getByRole("button", { name: "Back" }));
    await pickTarget(user, /^Worker/);
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Up to 400k on Worker / NVIDIA GB10")).toBeVisible();
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("65536");

    await user.click(screen.getByRole("button", { name: /Profile/ }));
    await user.click(screen.getByRole("radio", { name: "NVFP4 text" }));
    while (!screen.queryByRole("button", { name: "Deploy" }))
      await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));

    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    const { configuration, placement, profileId } = onSubmit.mock.calls[0][0];
    expect(profileId).toBe("nvfp4-text");
    expect(placement).toEqual(create(ExactPlacementSchema, workerPlacement));
    expect(
      pairs(configuration.engineParameterValues.values).find(([id]) => id === "total-context"),
    ).toEqual(["total-context", 65536n]);
  });

  it("opens a catalog 128k scenario on the Context step and submits it", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
    const catalogScenario: SizingScenario = { totalContextTokens: 131072 };
    renderDialog({
      initialProfileId: "q4-xl",
      listProfileCompatibility: scenarioLoader({}, [
        targetCompatibility(poolPlacement, {
          "q4-xl": { startingTotalContextTokens: 910336 },
        }),
      ]),
      model: longContextModel,
      onSubmit,
      scenario: catalogScenario,
    });

    await screen.findByRole("button", { name: "Runs on filter: this site, 128k" });
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("131072");
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));

    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(pairs(onSubmit.mock.calls[0][0].configuration.engineParameterValues.values)).toEqual([
      ["prompt-cache-memory-mib", 8192n],
      ["total-context", 131072n],
      ["parallel-requests", 8n],
    ]);
  });
});

describe("ModelDeployDialog scenario polling", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("keeps the first scenario values across polls and refreshes only the placement preview", async () => {
    vi.useFakeTimers();
    let estimates = 0;
    const onEstimate = vi.fn<ResourceEstimateLoader>(() =>
      Promise.resolve({
        // Another model takes 4 GiB of the GPU after the first read.
        placementPreview: placementPreview([
          devicePool(32n * gib, [other((estimates++ ? 16n : 12n) * gib), previewed(8n * gib)]),
        ]),
      }),
    );
    const onPreviewMemory = vi.fn().mockResolvedValue({
      acceleratorMemoryArchitecture: AcceleratorMemoryArchitecture.UNSPECIFIED,
      advisories: [],
    });
    const changed = {
      "ninfer-nvfp4": [
        ["parallel-requests", 2n],
        ["host-context-mib", 20480n],
        ["device-state-slots", 4n],
        ["total-context", 131072n],
        ["context-per-request", 65536n],
      ],
    } as const;
    const targets = [targetCompatibility(poolPlacement, { "ninfer-nvfp4": {} })];
    let reads = 0;
    const listProfileCompatibility = vi.fn(() =>
      Promise.resolve(scenarioResponse(targets, reads++ ? changed : scenarioValues)),
    );
    renderDialog({
      deployments: [],
      initialProfileId: "ninfer-nvfp4",
      listProfileCompatibility,
      onEstimate,
      onPreviewMemory,
      scenario,
    });

    await advance();
    expect(onEstimate).toHaveBeenCalledOnce();
    expect(onPreviewMemory).toHaveBeenCalledOnce();
    const bar = () =>
      within(screen.getByRole("region", { name: "Predicted memory" })).getByRole("img");
    expect(bar()).toHaveAttribute(
      "aria-label",
      "VRAM on node: 20 GiB used of 32 GiB; deployments: Qwen3.8 27B 8 GiB",
    );

    await advance(PROFILE_COMPATIBILITY_REFRESH_INTERVAL_MS);
    await advance(PROFILE_COMPATIBILITY_REFRESH_INTERVAL_MS);
    expect(listProfileCompatibility).toHaveBeenCalledTimes(3);
    expect(onEstimate).toHaveBeenCalledTimes(3);
    expect(onPreviewMemory).toHaveBeenCalledOnce();
    expect(bar()).toHaveAttribute(
      "aria-label",
      "VRAM on node: 24 GiB used of 32 GiB; deployments: Qwen3.8 27B 8 GiB",
    );
    for (const [call] of onEstimate.mock.calls) {
      expect(pairs(call.configuration.engineParameterValues.values)).toEqual(
        scenarioValues["ninfer-nvfp4"],
      );
    }
  });

  it("keeps the first fitting target when a later read ranks targets differently", async () => {
    vi.useFakeTimers();
    const onEstimate = vi.fn().mockResolvedValue({});
    let reads = 0;
    const listProfileCompatibility = vi.fn(() => {
      const flipped = reads++ > 0;
      return Promise.resolve(
        scenarioResponse(
          [
            targetCompatibility(poolPlacement, {
              "ninfer-nvfp4": { memoryFit: flipped ? MemoryFit.FITS : MemoryFit.EXCEEDS },
            }),
            targetCompatibility(workerPlacement, {
              "ninfer-nvfp4": { memoryFit: flipped ? MemoryFit.EXCEEDS : MemoryFit.FITS },
            }),
          ],
          scenarioValues,
        ),
      );
    });
    renderDialog({
      initialProfileId: "ninfer-nvfp4",
      listConfiguredExecutionTargets: () =>
        Promise.resolve({ executionTargets: [poolTarget(), workerTarget()] }),
      listProfileCompatibility,
      onEstimate,
      scenario,
    });

    await advance();
    expect(onEstimate).toHaveBeenCalledOnce();
    await advance(PROFILE_COMPATIBILITY_REFRESH_INTERVAL_MS);
    expect(listProfileCompatibility).toHaveBeenCalledTimes(2);
    // The poll refreshes the estimate on the same target.
    expect(onEstimate.mock.calls.map(([call]) => (call as EstimateCall).placement)).toEqual([
      create(ExactPlacementSchema, workerPlacement),
      create(ExactPlacementSchema, workerPlacement),
    ]);
  });

  it("keeps the first read's starting total context when later reads report another", async () => {
    vi.useFakeTimers();
    let reads = 0;
    const listProfileCompatibility = vi.fn(() => {
      const startingTotalContextTokens = reads++ > 0 ? 98304 : 126976;
      return Promise.resolve(
        scenarioResponse([
          targetCompatibility(poolPlacement, {
            "ninfer-nvfp4": { startingTotalContextTokens },
          }),
        ]),
      );
    });
    renderDialog({ initialProfileId: "ninfer-nvfp4", listProfileCompatibility });
    await advance();
    fireEvent.click(screen.getByRole("button", { name: "Next" }));
    const expectFirstFit = () => {
      expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("126976");
      expect(screen.getByText(/^Up to 124k on /)).toBeVisible();
      expect(screen.getByText("Largest fit")).toBeVisible();
    };
    expectFirstFit();

    await advance(PROFILE_COMPATIBILITY_REFRESH_INTERVAL_MS);

    expect(listProfileCompatibility).toHaveBeenCalledTimes(2);
    expectFirstFit();
  });

  it("shows the selected profile's frozen starting total context in the table, and other rows live", async () => {
    vi.useFakeTimers();
    let reads = 0;
    const listProfileCompatibility = vi.fn(() => {
      const later = reads++ > 0;
      return Promise.resolve(
        scenarioResponse([
          targetCompatibility(poolPlacement, {
            "ninfer-nvfp4": { startingTotalContextTokens: later ? 98304 : 126976 },
            "nvfp4-text": { startingTotalContextTokens: later ? 32768 : 65536 },
          }),
        ]),
      );
    });
    renderDialog({ initialProfileId: "ninfer-nvfp4", listProfileCompatibility });
    await advance();
    expect(totalContextCell("NVFP4 ninfer")).toBe("124k");
    expect(totalContextCell("NVFP4 text")).toBe("64k");

    await advance(PROFILE_COMPATIBILITY_REFRESH_INTERVAL_MS);

    expect(listProfileCompatibility).toHaveBeenCalledTimes(2);
    expect(totalContextCell("NVFP4 ninfer")).toBe("124k");
    expect(totalContextCell("NVFP4 text")).toBe("32k");
    fireEvent.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("126976");
  });

  it("applies the live starting total context on Use largest fit, and keeps it from then on", async () => {
    vi.useFakeTimers();
    // What every read reports from now on, as free memory changes.
    let live = 126976;
    const listProfileCompatibility = vi.fn(() =>
      Promise.resolve(
        scenarioResponse([
          targetCompatibility(poolPlacement, {
            "ninfer-nvfp4": { startingTotalContextTokens: live },
          }),
        ]),
      ),
    );
    renderDialog({ initialProfileId: "ninfer-nvfp4", listProfileCompatibility });
    await advance();
    fireEvent.click(screen.getByRole("button", { name: "Next" }));
    const total = () => screen.getByRole("textbox", { name: "Total context" });
    expect(total()).toHaveValue("126976");

    // A later read lowers the starting value; the unedited default stays frozen.
    live = 98304;
    const reads = listProfileCompatibility.mock.calls.length;
    await advance(PROFILE_COMPATIBILITY_REFRESH_INTERVAL_MS);
    expect(listProfileCompatibility.mock.calls.length).toBeGreaterThan(reads);
    expect(total()).toHaveValue("126976");

    fireEvent.click(
      within(screen.getByRole("group", { name: "Total context presets" })).getByRole("button", {
        name: "64k",
      }),
    );
    await advance();
    expect(total()).toHaveValue("65536");
    fireEvent.click(screen.getByRole("button", { name: "Use largest fit" }));
    await advance();
    expect(total()).toHaveValue("98304");
    expect(screen.getByText(/^Up to 96k on /)).toBeVisible();
    expect(screen.getByText("Largest fit")).toBeVisible();

    // The corrected value is frozen in turn.
    live = 65536;
    await advance(PROFILE_COMPATIBILITY_REFRESH_INTERVAL_MS);
    expect(total()).toHaveValue("98304");
    expect(screen.getByText(/^Up to 96k on /)).toBeVisible();
  });

  it("stays on the engine default when the first read has no fit and a later one does", async () => {
    vi.useFakeTimers();
    let reads = 0;
    const listProfileCompatibility = vi.fn(() => {
      const fit = reads++ > 0 ? 126976 : undefined;
      return Promise.resolve(
        scenarioResponse([
          targetCompatibility(poolPlacement, {
            "ninfer-nvfp4": {
              // Without a fit, the starting value is the profile's default.
              startingTotalContextSource: fit
                ? StartingTotalContextSource.LARGEST_FIT
                : StartingTotalContextSource.PROFILE_DEFAULT,
              startingTotalContextTokens: fit ?? 32768,
            },
          }),
        ]),
      );
    });
    renderDialog({ initialProfileId: "ninfer-nvfp4", listProfileCompatibility });
    await advance();
    fireEvent.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("32768");

    await advance(PROFILE_COMPATIBILITY_REFRESH_INTERVAL_MS);

    expect(listProfileCompatibility).toHaveBeenCalledTimes(2);
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("32768");
    expect(screen.queryByText(/^Up to /)).not.toBeInTheDocument();
  });

  it("keeps the operator's Runs on chip, place and selection across a refresh", async () => {
    vi.useFakeTimers();
    const listProfileCompatibility = scenarioLoader({}, twoTargetContexts);
    renderDialog({
      catalogChips: [{ field: "runsOn", values: ["node:worker"] }],
      initialProfileId: "ninfer-nvfp4",
      listConfiguredExecutionTargets: () =>
        Promise.resolve({ executionTargets: [poolTarget(), workerTarget()] }),
      listProfileCompatibility,
    });
    await advance();
    expect(screen.getByRole("button", { name: "Runs on filter: Worker" })).toBeVisible();
    expect(ninferTotalContext()).toBe("64k");

    fireEvent.click(screen.getByRole("button", { name: "Remove Runs on filter" }));
    expect(ninferTotalContext()).toBe("96k");

    await advance(PROFILE_COMPATIBILITY_REFRESH_INTERVAL_MS);
    expect(listProfileCompatibility).toHaveBeenCalledTimes(2);
    expect(screen.queryByRole("button", { name: /^Runs on filter/ })).not.toBeInTheDocument();
    expect(ninferTotalContext()).toBe("96k");
    expect(screen.getByRole("radio", { name: "NVFP4 ninfer" })).toBeChecked();
  });

  it("follows the starting chip again when a read without targets drops it", async () => {
    vi.useFakeTimers();
    let reads = 0;
    const listProfileCompatibility = vi.fn(() =>
      Promise.resolve(reads++ === 1 ? scenarioResponse([]) : scenarioResponse(twoTargetContexts)),
    );
    renderDialog({
      catalogChips: [{ field: "runsOn", values: ["node:worker"] }],
      initialProfileId: "ninfer-nvfp4",
      listConfiguredExecutionTargets: () =>
        Promise.resolve({ executionTargets: [poolTarget(), workerTarget()] }),
      listProfileCompatibility,
    });
    await advance();
    fireEvent.click(screen.getByRole("button", { name: "Remove Runs on filter" }));
    expect(ninferTotalContext()).toBe("96k");

    await advance(PROFILE_COMPATIBILITY_REFRESH_INTERVAL_MS);
    await advance(PROFILE_COMPATIBILITY_REFRESH_INTERVAL_MS);
    expect(listProfileCompatibility).toHaveBeenCalledTimes(3);
    // The table puts its starting chip back, and the dialog's place follows it.
    expect(screen.getByRole("button", { name: "Runs on filter: Worker" })).toBeVisible();
    expect(ninferTotalContext()).toBe("64k");
  });

  it("says the context couldn't be sized when its read fails, and starts the other values from the defaults", async () => {
    const onEstimate = vi.fn().mockResolvedValue({});
    renderDialog({
      initialProfileId: "ninfer-nvfp4",
      listProfileCompatibility: vi.fn(() => Promise.reject(new Error("read failed"))),
      onEstimate,
      scenario,
    });

    expect(
      await screen.findByText(
        "Couldn't size this context, so other settings start from the profile defaults.",
      ),
    ).toBeVisible();
    await waitFor(() => expect(onEstimate).toHaveBeenCalledOnce());
    expect(
      pairs(
        (onEstimate.mock.calls[0][0] as EstimateCall).configuration.engineParameterValues.values,
      ),
    ).toEqual([
      ["parallel-requests", 4n],
      ["host-context-mib", 20480n],
      ["device-state-slots", 4n],
      ["total-context", 524288n],
      ["context-per-request", 65536n],
    ]);
  });
});

const totalContextError = "Total context must be an integer from 1024 to 16777216 tokens.";

// The fixture model with a 256k maximum context, which caps the default total context.
const longContextModel = clone(ModelSchema, runtimeSchemaModel);
longContextModel.maximumContextTokens = 262144;

// NVFP4 ninfer fits up to 96k total context on the pool and 64k on the worker.
const twoTargetContexts = [
  targetCompatibility(poolPlacement, {
    "ninfer-nvfp4": { startingTotalContextTokens: 98304 },
  }),
  targetCompatibility(workerPlacement, {
    "ninfer-nvfp4": { startingTotalContextTokens: 65536 },
  }),
];

function ninferTotalContext(): string | null {
  return totalContextCell("NVFP4 ninfer");
}

function totalContextCell(profile: string): string | null {
  return within(screen.getByRole("row", { name: profile }))
    .getAllByRole("cell")
    .at(-1)!.textContent;
}

// The Deploy summary's context row.
function summaryContext(): string | null | undefined {
  const label = within(screen.getByRole("region", { name: "Summary" })).getByText("Context");
  return label.nextElementSibling?.textContent;
}

interface EstimateCall {
  configuration: { engineParameterValues: { values: EngineParameterValue[] } };
  placement: unknown;
  profileId: string;
}

function pairs(values: readonly EngineParameterValue[]) {
  return values.map((value) => [value.parameterId, value.value.value]);
}

/**
 * A compatibility loader whose read carries each profile's scenario values when asked for a
 * scenario, and none otherwise.
 */
function scenarioLoader(_options: object, targets: ReturnType<typeof targetCompatibility>[]) {
  return vi.fn((_modelId: string, requested?: SizingScenario) =>
    Promise.resolve(scenarioResponse(targets, requested ? scenarioValues : undefined)),
  );
}

/** A compatibility read over the targets, with each given profile's engine values. */
function scenarioResponse(
  targets: ReturnType<typeof targetCompatibility>[],
  values: Record<string, readonly (readonly [string, bigint])[]> = {},
) {
  return create(ListModelProfileCompatibilityResponseSchema, {
    ...compatibilityResponse(modelId, ...targets),
    profiles: Object.entries(values).map(([profileId, entries]) =>
      create(ModelProfileSizingSchema, {
        engineParameterValues: {
          values: entries.map(([parameterId, value]) => unsignedValue(parameterId, value)),
        },
        modelId,
        profileId,
      }),
    ),
  });
}

/** Moves from the Profile step to the Target step and picks the target the pattern names. */
async function pickTarget(user: ReturnType<typeof userEvent.setup>, name: RegExp) {
  if (!screen.queryByText(/— Target$/))
    await user.click(screen.getByRole("button", { name: "Next" }));
  await user.click(screen.getByRole("combobox", { name: "Execution target" }));
  await user.click(screen.getByRole("option", { name }));
}

/** Lets pending reads and the effects they trigger finish under fake timers. */
async function advance(ms = 0) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

// Picks a "Sized for" choice in the Runs on chip's menu, then closes the menu.
async function chooseSizedFor(user: UserEvent, chip: string, choice: "Custom" | "Defaults") {
  await user.click(await screen.findByRole("button", { name: chip }));
  await user.click(
    within(await screen.findByRole("menu")).getByRole("menuitemradio", { name: choice }),
  );
  await user.keyboard("{Escape}");
}

function renderDialog(props: Partial<ComponentProps<typeof ModelDeployDialog>> = {}) {
  return render(dialog(props));
}

function dialog(props: Partial<ComponentProps<typeof ModelDeployDialog>> = {}) {
  const listConfiguredExecutionTargets = props.listConfiguredExecutionTargets ?? poolTargets;
  return (
    <ExecutionTargetsWatch watch={executionTargetsWatch(listConfiguredExecutionTargets)}>
      <ModelDeployDialog
        engineReleases={runtimeSchemaReleases}
        hardwareClasses={[]}
        listConfiguredExecutionTargets={listConfiguredExecutionTargets}
        model={runtimeSchemaModel}
        onOpenChange={vi.fn()}
        onPreview={vi.fn()}
        onSubmit={vi.fn()}
        open
        pending={false}
        {...props}
      />
    </ExecutionTargetsWatch>
  );
}

// Profile rows by name, plus the text of any other body row.
function profileRows(): string[] {
  return within(screen.getByRole("table", { name: "Model profiles" }))
    .getAllByRole("row")
    .slice(1)
    .map((row) => row.getAttribute("aria-label") ?? row.textContent);
}

function profilesFooter(): string | null {
  return screen.getByText(/^\d+ of \d+ profiles?/).textContent;
}

/** The pool target, as the dialog's own execution targets read answers it. */
const poolTargets: ExecutionTargetsRead = () =>
  Promise.resolve({ executionTargets: [poolTarget()] });

function poolTarget() {
  return create(ConfiguredExecutionTargetSchema, {
    displayName: "NVIDIA GeForce RTX 5090",
    executionTargetId: "pool",
    siteNodeId: "node",
    workloadKind: ExecutionTargetWorkloadKind.MODEL,
    siteManager: true,
    availability: {
      connectionState: NodeObservationState.CONNECTED,
      runtimeAvailability: { availability: { case: "available", value: {} } },
    },
  });
}

function workerTarget() {
  return create(ConfiguredExecutionTargetSchema, {
    ...poolTarget(),
    displayName: "NVIDIA GB10",
    executionTargetId: "worker-gpu",
    siteManager: false,
    siteNodeDisplayName: "Worker",
    siteNodeId: "worker",
  });
}
