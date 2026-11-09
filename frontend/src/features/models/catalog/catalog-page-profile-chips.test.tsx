import { create } from "@bufbuild/protobuf";
import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import {
  currentLocation,
  modelTarget,
  renderModelsSection,
} from "@/features/models/deployed/models-section-test-support";
import {
  compatibilityResponse,
  driverTooOld,
  targetCompatibility,
} from "@/features/models/profiles/profile-compatibility-test-support";
import { allModels } from "@/features/models/profiles/use-profile-compatibility";
import {
  type ListModelProfileCompatibilityResponse,
  ListModelProfileCompatibilityResponseSchema,
} from "@/gen/dotcore/model/v1/service_pb";

import { syntheticCatalog } from "./catalog-table-test-support";

// Two machines: node-a runs every Alpha profile but Orphan, node-b only Fast. Of the Alpha
// profiles, Fast, Quality and cpu are llama.cpp, and only Fast and Quality take image input.
const gpuA = { executionTargetId: "gpu", siteNodeId: "node-a" };
const gpuB = { executionTargetId: "gpu", siteNodeId: "node-b" };
const notRunning = { reason: driverTooOld };

function siteRead(): ListModelProfileCompatibilityResponse {
  const alpha = compatibilityResponse(
    "alpha-7b",
    targetCompatibility(gpuA, { cpu: {}, fast: {}, orphan: notRunning, quality: {}, sized: {} }),
    targetCompatibility(gpuB, {
      cpu: notRunning,
      fast: {},
      orphan: notRunning,
      quality: notRunning,
      sized: notRunning,
    }),
  );
  const bare = compatibilityResponse(
    "bare",
    targetCompatibility(gpuA, { plain: {} }),
    targetCompatibility(gpuB, { plain: notRunning }),
  );
  return create(ListModelProfileCompatibilityResponseSchema, {
    entries: [...alpha.entries, ...bare.entries],
    targets: alpha.targets,
  });
}

function renderCatalog(
  initialEntry: string,
  read: (modelId: string) => Promise<ListModelProfileCompatibilityResponse> = () =>
    Promise.resolve(siteRead()),
) {
  const { engineReleases, hardwareClasses, models } = syntheticCatalog();
  return {
    ...renderModelsSection({
      compatibility: vi.fn(({ modelId = allModels }: { modelId?: string }) => read(modelId)),
      engineReleases,
      executionTargets: [
        modelTarget("node-a", "gpu", "GPU", "node-a"),
        modelTarget("node-b", "gpu", "GPU", "node-b"),
      ],
      hardwareClasses,
      initialEntry,
      models,
    }),
    user: userEvent.setup(),
  };
}

// The catalog narrowed to node-a, llama.cpp and image input, searching for Alpha.
const filtered =
  "/models/catalog?q=alpha&f=runs,engine,cap&runs=node%3Anode-a&engine=llama-cpp&cap=image";
const alphaPanel = "panel=model-catalog%2Fcatalog%3Aalpha-7b";

function panel() {
  return screen.getByRole("region", { name: "Profiles" });
}

// The chip buttons of a toolbar, by name.
function chips(container: HTMLElement): string[] {
  return within(container)
    .getAllByRole("button", { name: /^(?!Remove ).* filter: / })
    .map((button) => button.getAttribute("aria-label") ?? button.textContent);
}

function rows(container: HTMLElement): string[] {
  return within(within(container).getByRole("table", { name: "Model profiles" }))
    .getAllByRole("row")
    .slice(1)
    .map((row) => row.getAttribute("aria-label") ?? row.textContent);
}

describe("ModelCatalogPage profile tables open with the catalog's chips", () => {
  it("opens the model panel with the catalog's chips and an empty search, leaving the catalog alone", async () => {
    const { router, user } = renderCatalog(`${filtered}&${alphaPanel}`);
    await waitFor(() => expect(rows(panel())).toEqual(["Fast", "Quality"]));
    expect(chips(panel())).toEqual([
      "Runs on filter: node-a",
      "Engine filter: llama.cpp",
      "Capabilities filter: Image input",
    ]);
    expect(within(panel()).getByRole("searchbox", { name: "Filter profiles" })).toHaveValue("");
    const location = currentLocation(router);

    await user.click(within(panel()).getByRole("button", { name: "Remove Engine filter" }));
    expect(rows(panel())).toEqual(["Fast", "Quality"]);
    await user.click(within(panel()).getByRole("button", { name: "Remove Capabilities filter" }));
    expect(rows(panel())).toEqual(["Sized", "cpu", "Fast", "Quality"]);
    expect(currentLocation(router)).toBe(location);

    await user.click(within(panel()).getByRole("button", { name: "Reset" }));
    expect(chips(panel())).toEqual([
      "Runs on filter: node-a",
      "Engine filter: llama.cpp",
      "Capabilities filter: Image input",
    ]);
    expect(rows(panel())).toEqual(["Fast", "Quality"]);
  });

  it("starts again from the catalog's chips when switching models or reopening", async () => {
    const { router, user } = renderCatalog(`${filtered}&${alphaPanel}`);
    await waitFor(() => expect(rows(panel())).toEqual(["Fast", "Quality"]));
    await user.click(within(panel()).getByRole("button", { name: "Remove Engine filter" }));
    expect(chips(panel())).toEqual(["Runs on filter: node-a", "Capabilities filter: Image input"]);

    await act(() => router.navigate(`${filtered}&panel=model-catalog%2Fcatalog%3Abare`));
    await waitFor(() => expect(rows(panel())).toEqual(["No profiles match. Reset filters"]));
    expect(chips(panel())).toEqual([
      "Runs on filter: node-a",
      "Engine filter: llama.cpp",
      "Capabilities filter: Image input",
    ]);

    await act(() => router.navigate(filtered));
    await waitFor(() =>
      expect(screen.queryByRole("region", { name: "Profiles" })).not.toBeInTheDocument(),
    );
    await act(() => router.navigate(`${filtered}&${alphaPanel}`));
    await waitFor(() => expect(rows(panel())).toEqual(["Fast", "Quality"]));
    expect(chips(panel())).toEqual([
      "Runs on filter: node-a",
      "Engine filter: llama.cpp",
      "Capabilities filter: Image input",
    ]);
  });

  it("opens the panel by a direct link at this site with no other chips", async () => {
    renderCatalog(`/models/catalog?${alphaPanel}`);

    await waitFor(() => expect(chips(panel())).toEqual(["Runs on filter: this site"]));
    expect(rows(panel())).toEqual(["Sized", "cpu", "Fast", "Quality"]);
  });

  it("opens the deploy dialog with the catalog's chips, Runs on arriving with its read", async () => {
    let release = () => undefined as void;
    const modelRead = new Promise<void>((resolve) => {
      release = resolve;
    });
    const { router, user } = renderCatalog(`${filtered}&${alphaPanel}`, async (modelId) => {
      if (modelId !== allModels) await modelRead;
      return siteRead();
    });
    await waitFor(() => expect(rows(panel())).toEqual(["Fast", "Quality"]));
    const location = currentLocation(router);

    await user.click(screen.getByRole("button", { name: "Deploy" }));
    const dialog = await screen.findByRole("dialog", { name: /Deploy model/ });
    expect(chips(dialog)).toEqual(["Engine filter: llama.cpp", "Capabilities filter: Image input"]);
    expect(within(dialog).getByRole("searchbox", { name: "Filter profiles" })).toHaveValue("");

    act(() => release());
    await waitFor(() =>
      expect(chips(dialog)).toEqual([
        "Runs on filter: node-a",
        "Engine filter: llama.cpp",
        "Capabilities filter: Image input",
      ]),
    );
    expect(rows(dialog)).toEqual(["Fast", "Quality"]);

    await user.click(within(dialog).getByRole("button", { name: "Remove Runs on filter" }));
    expect(chips(dialog)).toEqual(["Engine filter: llama.cpp", "Capabilities filter: Image input"]);
    expect(currentLocation(router)).toBe(location);
    await user.click(within(dialog).getByRole("button", { name: "Reset" }));
    expect(chips(dialog)).toEqual([
      "Runs on filter: node-a",
      "Engine filter: llama.cpp",
      "Capabilities filter: Image input",
    ]);
  });

  it("opens Deploy from the panel with the panel's copied chips, not the catalog's later ones", async () => {
    const { router, user } = renderCatalog(`${filtered}&${alphaPanel}`);
    await waitFor(() => expect(rows(panel())).toEqual(["Fast", "Quality"]));

    // The catalog changes while the panel stays open on Alpha.
    await act(() => router.navigate(`/models/catalog?f=engine&engine=vllm&${alphaPanel}`));
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    const dialog = await screen.findByRole("dialog", { name: /Deploy model/ });
    await waitFor(() =>
      expect(chips(dialog)).toEqual([
        "Runs on filter: node-a",
        "Engine filter: llama.cpp",
        "Capabilities filter: Image input",
      ]),
    );
  });

  it("opens Deploy on a profile the chips would hide with that profile selected and visible", async () => {
    const { user } = renderCatalog(
      `${filtered}&${alphaPanel}&panel=model-catalog%2Fcatalog-profile%3Aalpha-7b%3Asized`,
    );

    await user.click(await screen.findByRole("button", { name: "Deploy" }));
    const dialog = await screen.findByRole("dialog", { name: /Deploy model/ });
    await waitFor(() => expect(chips(dialog)).toEqual(["Runs on filter: node-a"]));
    expect(within(dialog).getByRole("radio", { name: "Sized" })).toBeChecked();
  });
});
