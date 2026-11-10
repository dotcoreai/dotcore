import { clone, create } from "@bufbuild/protobuf";
import { render, screen, within } from "@testing-library/react";
import { createRef } from "react";
import { MemoryRouter } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";

import {
  nvidiaEstimates,
  syntheticCatalog,
} from "@/features/models/catalog/catalog-table-test-support";
import {
  profileEstimateKey,
  type ProfileEstimates,
  projectModelRows,
} from "@/features/models/profiles/model-row";
import type { CatalogProfileCompatibility } from "@/features/models/profiles/profile-availability";
import { indexProfileCompatibility } from "@/features/models/profiles/profile-compatibility";
import {
  compatibilityResponse,
  driverTooOld,
  notReporting,
  targetCompatibility,
} from "@/features/models/profiles/profile-compatibility-test-support";
import type { CatalogChips } from "@/features/models/profiles/profile-table-seed";
import { EngineArtifactReferenceSchema } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import { type Model, ModelSchema } from "@/gen/dotcore/catalog/v1/model_pb";
import { MemoryFit } from "@/gen/dotcore/deployment/v1/memory_fit_pb";
import {
  type ListModelProfileCompatibilityResponse,
  LocalModelDeploymentAdvisory,
  ModelCompatibilityReasonCode,
} from "@/gen/dotcore/model/v1/service_pb";
import {
  ConfiguredExecutionTargetSchema,
  ExecutionTargetWorkloadKind,
} from "@/gen/dotcore/site/v1/service_pb";
import { setupUser } from "@/test/user";

import { CatalogModelInspector, type CatalogModelInspectorScreen } from "./catalog-model-inspector";

const { engineReleases, hardwareClasses, models } = syntheticCatalog();
const alpha = models[0];

const workstation = { executionTargetId: "gpu", siteNodeId: "ws" };
const lab = { executionTargetId: "gpu", siteNodeId: "lab" };
const mac = { executionTargetId: "metal", siteNodeId: "mac" };

const configuredTargets = [
  configuredTarget(workstation, "Workstation", "NVIDIA GeForce RTX 5090", "linux/amd64", 1),
  configuredTarget(lab, "Lab", "2× NVIDIA GeForce RTX 4090", "linux/amd64", 2),
  configuredTarget(mac, "Mac Studio", "Apple M3 Ultra", "darwin/arm64", 0),
];

const tooManyDevices = {
  acceleratorDeviceCount: 2,
  code: ModelCompatibilityReasonCode.ACCELERATOR_DEVICE_COUNT_MISMATCH,
  requiredAcceleratorDeviceCount: 1,
};

// Fast runs on the workstation, cpu on the lab, Quality on both, Sized on the workstation with an
// advisory, and Orphan nowhere.
const gpuMachines = [
  targetCompatibility(workstation, {
    cpu: { reason: { code: ModelCompatibilityReasonCode.PROFILE_UNRESOLVABLE } },
    fast: {},
    orphan: { reason: driverTooOld },
    quality: {},
    sized: { advisories: [LocalModelDeploymentAdvisory.HOST_KV_OFFLOAD_SHARES_MEMORY] },
  }),
  targetCompatibility(lab, {
    cpu: {},
    fast: { reason: tooManyDevices },
    orphan: { reason: driverTooOld },
    quality: {},
    sized: { reason: driverTooOld },
  }),
];
const twoMachines = compatibilityResponse("alpha-7b", ...gpuMachines);
// The same machines and a Mac that isn't reporting.
const threeMachines = compatibilityResponse("alpha-7b", ...gpuMachines, notReporting(mac));

// Under a scenario on the workstation alone: Fast exceeds memory, Quality fits unless `quality`
// says otherwise, and the other profiles can't run.
function scenarioRead(quality = MemoryFit.FITS) {
  return compatibilityResponse(
    "alpha-7b",
    targetCompatibility(workstation, {
      cpu: { reason: driverTooOld },
      fast: { memoryFit: MemoryFit.EXCEEDS },
      orphan: { reason: driverTooOld },
      quality: { memoryFit: quality },
      sized: { reason: driverTooOld },
    }),
  );
}

function renderInspector({
  catalogChips,
  compatibility,
  estimates = nvidiaEstimates([["alpha-7b", "fast", 7_500_000_000n]]),
  memoryPending = false,
  model = alpha,
  profileId,
  screen: inspectorScreen = "overview",
}: {
  catalogChips?: CatalogChips;
  compatibility?: CatalogProfileCompatibility;
  estimates?: ProfileEstimates;
  memoryPending?: boolean;
  model?: Model;
  profileId?: string;
  screen?: CatalogModelInspectorScreen;
} = {}) {
  const [row] = projectModelRows({
    deployments: [],
    engineReleases,
    hardwareClasses,
    estimates,
    models: [model],
  });
  const onDeploy = vi.fn();
  const onSelectProfile = vi.fn<(profileId: string) => void>();
  const inspector = (shown: CatalogProfileCompatibility | undefined) => (
    <MemoryRouter>
      <CatalogModelInspector
        catalogChips={catalogChips}
        compatibility={shown}
        memoryPending={memoryPending}
        model={model}
        onDeploy={onDeploy}
        onOpenChange={vi.fn()}
        onSelectProfile={onSelectProfile}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
        profileId={profileId}
        row={row}
        screen={inspectorScreen}
      />
    </MemoryRouter>
  );
  const { rerender } = render(inspector(compatibility));
  return {
    onDeploy,
    onSelectProfile,
    /** Shows a later compatibility read, as a refresh does. */
    refresh: (next: CatalogProfileCompatibility) => rerender(inspector(next)),
    user: setupUser(),
  };
}

/** The panel's view of a compatibility read over the configured targets. */
function readOf(
  response: ListModelProfileCompatibilityResponse,
  sizing: Pick<CatalogProfileCompatibility, "sizingFailed"> = {
    sizingFailed: false,
  },
): CatalogProfileCompatibility {
  return {
    compatibility: indexProfileCompatibility(response),
    loading: false,
    targets: configuredTargets,
    ...sizing,
  };
}

function details(region: HTMLElement): string[][] {
  const terms = within(region).getAllByRole("term");
  return terms.map((term) => [term.textContent, term.nextElementSibling!.textContent]);
}

function profilesTable() {
  return screen.getByRole("table", { name: "Model profiles" });
}

// Profile rows by name, plus the text of any other body row.
function profileRows(): string[] {
  return within(profilesTable())
    .getAllByRole("row")
    .slice(1)
    .map((row) => row.getAttribute("aria-label") ?? row.textContent);
}

function profilesFooter(): string {
  return screen.getByText(/^\d+ of \d+ profiles?/).textContent;
}

// Each Runs on row's machine cell and status pill.
function runsOnRows(): string[][] {
  const table = within(screen.getByRole("region", { name: "Runs on" })).getByRole("table");
  return within(table)
    .getAllByRole("row")
    .slice(1)
    .map((row) =>
      within(row)
        .getAllByRole("cell")
        .map((cell) => cell.textContent),
    );
}

function configuredTarget(
  target: { executionTargetId: string; siteNodeId: string },
  siteNodeDisplayName: string,
  displayName: string,
  platformLabel: string,
  devices: number,
) {
  return create(ConfiguredExecutionTargetSchema, {
    ...target,
    displayName,
    acceleratorDeviceIds: Array.from({ length: devices }, (_, index) => `nvidia.com/gpu=${index}`),
    platformLabel,
    siteNodeDisplayName,
    workloadKind: ExecutionTargetWorkloadKind.MODEL,
  });
}

describe("Catalog model panel profiles", () => {
  it("lists the profiles in a Profile and Memory table that opens each profile", async () => {
    const { onSelectProfile, user } = renderInspector();

    expect(
      within(profilesTable())
        .getAllByRole("columnheader")
        .map((header) => header.textContent),
    ).toEqual(["Profile", "Memory"]);
    expect(profileRows()).toEqual(["Sized", "cpu", "Quality", "Fast", "Orphan"]);
    expect(
      within(screen.getByRole("row", { name: "Fast" }))
        .getAllByRole("cell")
        .map((cell) => cell.textContent),
    ).toEqual(["Fastllama.cppMTPNVIDIAQ4_K_M", "7 GiB"]);
    expect(screen.queryByRole("button", { name: "View" })).not.toBeInTheDocument();
    expect(screen.queryByRole("radio")).not.toBeInTheDocument();

    await user.click(screen.getByRole("row", { name: "Quality" }));
    expect(onSelectProfile).toHaveBeenCalledWith("quality");
  });

  it("shows a pending memory placeholder while estimates load", () => {
    renderInspector({ memoryPending: true });

    expect(
      within(screen.getByRole("row", { name: "Fast" })).getByText("Loading estimate"),
    ).toBeInTheDocument();
  });

  it("shows the profiles that run on the site behind a removable chip", async () => {
    const { user } = renderInspector({ compatibility: readOf(twoMachines) });

    expect(screen.getByRole("button", { name: "Runs on filter: this site" })).toBeVisible();
    expect(profileRows()).toEqual(["Sized", "cpu", "Quality", "Fast"]);
    // A profile that runs shows no status line, even with an advisory.
    expect(
      within(screen.getByRole("row", { name: "Sized" })).getByRole("cell", { name: /Sized/ }),
    ).toHaveTextContent(/^SizedvLLMNVIDIANVFP4$/);
    expect(profilesFooter()).toBe("4 of 5 profiles");

    await user.click(screen.getByRole("button", { name: "Remove Runs on filter" }));
    expect(screen.queryByRole("button", { name: /^Runs on filter/ })).not.toBeInTheDocument();
    expect(profileRows()).toEqual(["Sized", "cpu", "Quality", "Fast", "Orphan"]);
    expect(
      within(screen.getByRole("row", { name: "Orphan" })).getByText(
        "Needs NVIDIA driver 575 or later (has 560)",
      ),
    ).toHaveClass("text-destructive");
    expect(profilesFooter()).toBe("5 of 5 profiles");
  });

  it("narrows Runs on to one machine", async () => {
    const { user } = renderInspector({ compatibility: readOf(threeMachines) });

    await user.click(screen.getByRole("button", { name: "Runs on filter: this site" }));
    const menu = await screen.findByRole("menu");
    expect(
      within(menu)
        .getAllByRole("menuitemcheckbox")
        .map((item) => item.textContent),
    ).toEqual(["this site4", "Workstation3", "Lab2", "Mac Studio0"]);
    await user.click(within(menu).getByRole("menuitemcheckbox", { name: /^Lab/ }));
    await user.keyboard("{Escape}");

    expect(screen.getByRole("button", { name: "Runs on filter: Lab" })).toBeVisible();
    expect(profileRows()).toEqual(["cpu", "Quality"]);
    expect(profilesFooter()).toBe("2 of 5 profiles");
  });

  it("says why no profile runs on the site and offers every profile", async () => {
    const { user } = renderInspector({
      compatibility: readOf(compatibilityResponse("alpha-7b", notReporting(mac))),
    });

    expect(profileRows()).toEqual([
      "No profile runs hereMac Studio isn't reportingShow all profiles",
    ]);

    await user.click(screen.getByRole("button", { name: "Show all profiles" }));
    expect(profileRows()).toEqual(["Sized", "cpu", "Quality", "Fast", "Orphan"]);
    expect(
      within(screen.getByRole("row", { name: "Fast" })).getByText("Mac Studio isn't reporting"),
    ).toHaveClass("text-muted-foreground");
  });

  it("lists every profile without the chip on a site without model execution targets", () => {
    renderInspector({ compatibility: readOf(compatibilityResponse("alpha-7b")) });

    expect(screen.queryByRole("button", { name: /^Runs on filter/ })).not.toBeInTheDocument();
    expect(profileRows()).toEqual(["Sized", "cpu", "Quality", "Fast", "Orphan"]);
    expect(profilesFooter()).toBe("5 of 5 profiles");
  });

  it("hides a profile that exceeds memory under the scenario and counts it", async () => {
    const { user } = renderInspector({
      compatibility: readOf(scenarioRead(), { sizingFailed: false }),
    });

    expect(profileRows()).toEqual(["Quality"]);
    expect(profilesFooter()).toBe("1 of 5 profiles · 1 hidden");

    await user.click(screen.getByRole("button", { name: "Remove Runs on filter" }));
    expect(profileRows()).toEqual(["Quality", "Fast", "Sized", "cpu", "Orphan"]);
    expect(profilesFooter()).toBe("5 of 5 profiles");
  });

  it("says no profile runs when every running one exceeds", () => {
    renderInspector({
      compatibility: readOf(scenarioRead(MemoryFit.EXCEEDS), { sizingFailed: false }),
    });

    expect(profileRows()).toEqual([
      "No profile runs hereNeeds NVIDIA driver 575 or later (has 560)Show all profiles",
    ]);
    expect(profilesFooter()).toBe("0 of 5 profiles · 2 hidden");
  });

  it("judges support alone once the scenario's read failed", () => {
    renderInspector({
      compatibility: readOf(scenarioRead(), { sizingFailed: true }),
    });

    expect(profileRows()).toEqual(["Quality", "Fast"]);
    expect(profilesFooter()).toBe("2 of 5 profiles");
  });

  it("keeps the operator's chip when a later read offers a copied machine the first didn't", async () => {
    const { refresh, user } = renderInspector({
      catalogChips: [{ field: "runsOn", values: ["node:mac"] }],
      compatibility: readOf(twoMachines),
    });
    // The Mac isn't in the first read, so the chip starts at this site.
    expect(screen.getByRole("button", { name: "Runs on filter: this site" })).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Runs on filter: this site" }));
    const menu = await screen.findByRole("menu");
    await user.click(within(menu).getByRole("menuitemcheckbox", { name: /^Lab/ }));
    await user.keyboard("{Escape}");
    expect(profileRows()).toEqual(["cpu", "Quality"]);

    refresh(readOf(threeMachines));
    expect(screen.getByRole("button", { name: "Runs on filter: Lab" })).toBeVisible();
    expect(profileRows()).toEqual(["cpu", "Quality"]);
  });

  it("lists every profile without the chip when the read fails", () => {
    renderInspector({
      compatibility: { error: "unavailable", loading: false, sizingFailed: false, targets: [] },
    });

    expect(screen.getByText("Couldn't check which profiles can run on this site.")).toBeVisible();
    expect(screen.queryByRole("button", { name: /^Runs on filter/ })).not.toBeInTheDocument();
    expect(profileRows()).toEqual(["Sized", "cpu", "Quality", "Fast", "Orphan"]);
  });
});

describe("Catalog profile sub-screen", () => {
  it("shows the fields of a GPU profile in one Profile section", () => {
    renderInspector({ profileId: "fast", screen: "profile-detail" });

    const profile = screen.getByRole("region", { name: "Profile" });
    expect(within(profile).getByTitle("Inference engine")).toHaveTextContent("llama.cpp");
    expect(within(profile).getByText("Starts quickly.")).toBeVisible();
    expect(details(profile)).toEqual([
      ["Engine", "llama.cpp"],
      ["Quantization", "Q4_K_M"],
      ["Memory", "7 GiB"],
      ["Maximum context", "256k"],
      ["Total context", "—"],
      ["Speculative decoding", "MTP · 0–8 tokens"],
      ["Speculative default", "MTP, 3 tokens"],
      ["Platforms", "linux/amd64, linux/arm64"],
      ["Accelerator", "NVIDIA, ≥7.5 GiB per device"],
      ["Engine release", "llama-1"],
      ["Profile ID", "fast"],
    ]);
    expect(screen.queryByRole("region", { name: "Configuration" })).not.toBeInTheDocument();
  });

  it("shows each hardware class's estimate and every approved engine release", () => {
    const model = clone(ModelSchema, alpha);
    model.configurations
      .find((configuration) => configuration.configurationId === "cuda-llama")!
      .engineArtifacts.push(
        create(EngineArtifactReferenceSchema, {
          engineArtifactId: "darwin-arm64-metal",
          engineReleaseId: "llama-2",
        }),
      );
    renderInspector({
      estimates: new Map([
        [
          profileEstimateKey("alpha-7b", "fast"),
          [
            { acceleratorMemoryBytes: 8n * 1024n ** 3n, hardwareClassId: "nvidia" },
            {
              acceleratorMemoryBytes: 6n * 1024n ** 3n,
              hardwareClassId: "apple",
              hostMemoryBytes: 2n * 1024n ** 3n,
            },
          ],
        ],
      ]),
      model,
      profileId: "fast",
      screen: "profile-detail",
    });

    const rows = details(screen.getByRole("region", { name: "Profile" }));
    expect(rows[2]).toEqual(["Memory", "NVIDIA 8 GiBapple 6 GiB + 2 GiB host RAM"]);
    expect(rows[9]).toEqual(["Engine release", "llama-1, llama-2"]);
  });

  it("shows the fields of a CPU-only profile", () => {
    renderInspector({ profileId: "cpu", screen: "profile-detail" });

    const profile = screen.getByRole("region", { name: "Profile" });
    expect(details(profile)).toEqual([
      ["Engine", "llama.cpp"],
      ["Quantization", "Q4_K_M"],
      ["Memory", "14.9 GiB system RAM"],
      ["Maximum context", "256k"],
      ["Total context", "—"],
      ["Platforms", "linux/amd64"],
      ["Accelerator", "None"],
      ["Engine release", "llama-1"],
      ["Profile ID", "cpu"],
    ]);
  });

  it("shows the per-device requirement of the supported hardware class", () => {
    renderInspector({ profileId: "sized", screen: "profile-detail" });

    const rows = details(screen.getByRole("region", { name: "Profile" }));
    expect(rows[2]).toEqual(["Memory", "≥22.4 GiB"]);
    expect(rows[3]).toEqual(["Maximum context", "64k"]);
    expect(rows[6]).toEqual(["Accelerator", "NVIDIA, ≥22.4 GiB per device"]);
  });

  it("shows the absent values of a profile whose engine release is missing", () => {
    renderInspector({ profileId: "orphan", screen: "profile-detail" });

    expect(details(screen.getByRole("region", { name: "Profile" }))).toEqual([
      ["Engine", "vLLM"],
      ["Quantization", "Q8_0"],
      ["Memory", "≥5.6 GiB"],
      ["Maximum context", "256k"],
      ["Total context", "—"],
      ["Platforms", "Unavailable"],
      ["Accelerator", "NVIDIA, ≥5.6 GiB per device"],
      ["Engine release", "gone-1"],
      ["Profile ID", "orphan"],
    ]);
  });

  it("lists the machines that run the profile first, each with its status", async () => {
    const { onDeploy, user } = renderInspector({
      compatibility: readOf(threeMachines),
      profileId: "sized",
      screen: "profile-detail",
    });

    const runsOn = screen.getByRole("region", { name: "Runs on" });
    expect(within(runsOn).getByText("1 of 3 can run")).toBeVisible();
    expect(runsOnRows()).toEqual([
      [
        "Workstation / NVIDIA GeForce RTX 5090linux/amd64 · 1 GPUKV cache offload adds no capacity on this target",
        "Runs",
      ],
      ["Mac Studio / Apple M3 Ultradarwin/arm64Mac Studio isn't reporting", "Unknown"],
      [
        "Lab / 2× NVIDIA GeForce RTX 4090linux/amd64 · 2 GPUsNeeds NVIDIA driver 575 or later (has 560)",
        "Can't run",
      ],
    ]);

    await user.click(screen.getByRole("button", { name: "Deploy" }));
    expect(onDeploy).toHaveBeenCalledOnce();
  });

  it("lists each GPU target of one machine on its own row", () => {
    const gpu0 = { executionTargetId: "rig-gpu0", siteNodeId: "rig" };
    const gpu1 = { executionTargetId: "rig-gpu1", siteNodeId: "rig" };
    const pair = { executionTargetId: "rig-pair", siteNodeId: "rig" };
    const rigTargets = [
      configuredTarget(gpu0, "Rig", "NVIDIA GeForce RTX 5090 · GPU 0", "linux/amd64", 1),
      configuredTarget(gpu1, "Rig", "NVIDIA GeForce RTX 5090 · GPU 1", "linux/amd64", 1),
      configuredTarget(pair, "Rig", "2× NVIDIA GeForce RTX 5090", "linux/amd64", 2),
    ];
    renderInspector({
      compatibility: {
        compatibility: indexProfileCompatibility(
          compatibilityResponse(
            "alpha-7b",
            targetCompatibility(gpu0, { fast: {} }),
            targetCompatibility(gpu1, { fast: {} }),
            targetCompatibility(pair, { fast: { reason: tooManyDevices } }),
          ),
        ),
        loading: false,
        sizingFailed: false,
        targets: rigTargets,
      },
      profileId: "fast",
      screen: "profile-detail",
    });

    expect(runsOnRows()).toEqual([
      ["Rig / NVIDIA GeForce RTX 5090 · GPU 0linux/amd64 · 1 GPU", "Runs"],
      ["Rig / NVIDIA GeForce RTX 5090 · GPU 1linux/amd64 · 1 GPU", "Runs"],
      [
        "Rig / 2× NVIDIA GeForce RTX 5090linux/amd64 · 2 GPUsNeeds 1 GPU, target has 2",
        "Can't run",
      ],
    ]);
  });

  it("disables Deploy when no machine can run the profile", () => {
    renderInspector({
      compatibility: readOf(
        compatibilityResponse(
          "alpha-7b",
          targetCompatibility(workstation, { orphan: { reason: driverTooOld } }),
          targetCompatibility(lab, { orphan: { reason: driverTooOld } }),
        ),
      ),
      profileId: "orphan",
      screen: "profile-detail",
    });

    expect(runsOnRows().map(([, status]) => status)).toEqual(["Can't run", "Can't run"]);
    const deploy = screen.getByRole("button", { name: "Deploy" });
    expect(deploy).toHaveAttribute("aria-disabled", "true");
    expect(deploy).toHaveAccessibleDescription("No machine on this site can run this profile");
  });

  it("disables Deploy when every machine is unknown", () => {
    renderInspector({
      compatibility: readOf(compatibilityResponse("alpha-7b", notReporting(mac))),
      profileId: "fast",
      screen: "profile-detail",
    });

    expect(runsOnRows()).toEqual([
      ["Mac Studio / Apple M3 Ultradarwin/arm64Mac Studio isn't reporting", "Unknown"],
    ]);
    const deploy = screen.getByRole("button", { name: "Deploy" });
    expect(deploy).toHaveAttribute("aria-disabled", "true");
    expect(deploy).toHaveAccessibleDescription(
      "No machine on this site can run this profile right now",
    );
  });

  it("keeps Deploy enabled without statuses when the read fails", () => {
    renderInspector({
      compatibility: {
        error: "unavailable",
        loading: false,
        sizingFailed: false,
        targets: configuredTargets,
      },
      profileId: "fast",
      screen: "profile-detail",
    });

    const runsOn = screen.getByRole("region", { name: "Runs on" });
    expect(runsOn).toHaveTextContent("Couldn't check which machines can run this profile.");
    expect(within(runsOn).queryByRole("table")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Deploy" })).toBeEnabled();
  });

  it("says when the site has no model execution targets and keeps Deploy enabled", () => {
    renderInspector({
      compatibility: readOf(compatibilityResponse("alpha-7b")),
      profileId: "fast",
      screen: "profile-detail",
    });

    expect(screen.getByRole("region", { name: "Runs on" })).toHaveTextContent(
      "No model execution targets are configured. Add one to the site configuration to deploy.",
    );
    expect(screen.getByRole("button", { name: "Deploy" })).toBeEnabled();
  });
});
