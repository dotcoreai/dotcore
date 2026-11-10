import { create } from "@bufbuild/protobuf";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import {
  currentLocation,
  renderModelsSection,
} from "@/features/models/deployed/models-section-test-support";
import {
  compatibilityResponse,
  driverTooOld,
  notReporting,
  profileSizing,
  targetCompatibility,
} from "@/features/models/profiles/profile-compatibility-test-support";
import {
  type ListModelProfileCompatibilityResponse,
  ListModelProfileCompatibilityResponseSchema,
} from "@/gen/dotcore/model/v1/service_pb";

import { syntheticCatalog } from "./catalog-table-test-support";

// The configured target of the test site is "pool" on "node"; "node-b" isn't reporting. Of the
// synthetic Alpha profiles only fast and quality run, and Bare can't run.
const pool = { executionTargetId: "pool", siteNodeId: "node" };
const silent = { executionTargetId: "pool", siteNodeId: "node-b" };

function siteRead(): ListModelProfileCompatibilityResponse {
  const alpha = compatibilityResponse(
    "alpha-7b",
    targetCompatibility(pool, {
      cpu: { reason: driverTooOld },
      fast: {},
      orphan: { reason: driverTooOld },
      quality: {},
      sized: { reason: driverTooOld },
    }),
    notReporting(silent),
  );
  const bare = compatibilityResponse(
    "bare",
    targetCompatibility(pool, { plain: { reason: driverTooOld } }),
    notReporting(silent),
  );
  return create(ListModelProfileCompatibilityResponseSchema, {
    entries: [...alpha.entries, ...bare.entries],
    targets: alpha.targets,
  });
}

function renderCatalog(
  initialEntry: string,
  compatibility: Parameters<typeof renderModelsSection>[0]["compatibility"] = vi.fn(() =>
    Promise.resolve(siteRead()),
  ),
) {
  const { engineReleases, hardwareClasses, models } = syntheticCatalog();
  return {
    ...renderModelsSection({
      compatibility,
      engineReleases,
      hardwareClasses,
      initialEntry,
      models,
    }),
    user: userEvent.setup(),
  };
}

function table() {
  return screen.getByRole("table", { name: "Model catalog" });
}

function modelRows(): string[] {
  return within(table())
    .getAllByRole("row")
    .slice(1)
    .map((row) => row.getAttribute("aria-label") ?? row.textContent);
}

describe("ModelCatalogPage Runs on", () => {
  it("starts with the models and profiles that run on this site behind a removable chip", async () => {
    const { router, user } = renderCatalog("/models/catalog");

    expect(await screen.findByRole("button", { name: "Runs on filter: this site" })).toBeVisible();
    expect(modelRows()).toEqual(["Alpha 7B"]);
    expect(
      within(screen.getByRole("row", { name: "Alpha 7B" })).getAllByRole("cell")[0],
    ).toHaveTextContent("Alpha 7BVision · Chat, Decisions · 2 of 5 profiles");
    expect(screen.getByText("1 of 2 models")).toBeVisible();
    expect(currentLocation(router)).toBe("/models/catalog");

    await user.click(screen.getByRole("button", { name: "Remove Runs on filter" }));
    expect(currentLocation(router)).toBe("/models/catalog?f=");
    expect(modelRows()).toEqual(["Alpha 7B", "Bare"]);
    expect(screen.getByText("2 of 2 models")).toBeVisible();
  });

  it("keeps the chip removed after a reload, and shows every model", async () => {
    renderCatalog("/models/catalog?f=");

    await screen.findByRole("table", { name: "Model catalog" });
    await waitFor(() => expect(modelRows()).toEqual(["Alpha 7B", "Bare"]));
    expect(screen.queryByRole("button", { name: /^Runs on filter/ })).not.toBeInTheDocument();
  });

  it("keeps a removed chip while a stale panel is dropped before the read lands", async () => {
    let finish: () => void = () => undefined;
    const read = new Promise<ListModelProfileCompatibilityResponse>((resolve) => {
      finish = () => resolve(siteRead());
    });
    const { router } = renderCatalog(
      "/models/catalog?f=&panel=model-catalog%2Fcatalog%3Amissing",
      vi.fn(() => read),
    );

    await waitFor(() => expect(currentLocation(router)).toBe("/models/catalog?f="));
    finish();
    await screen.findByRole("table", { name: "Model catalog" });
    await waitFor(() => expect(modelRows()).toEqual(["Alpha 7B", "Bare"]));
    expect(currentLocation(router)).toBe("/models/catalog?f=");
    expect(screen.queryByRole("button", { name: /^Runs on filter/ })).not.toBeInTheDocument();
  });

  it("lists a machine with several GPU targets once, running what any of its targets runs", async () => {
    const gpu0 = { executionTargetId: "rig-gpu0", siteNodeId: "rig" };
    const gpu1 = { executionTargetId: "rig-gpu1", siteNodeId: "rig" };
    const read = compatibilityResponse(
      "alpha-7b",
      targetCompatibility(gpu0, { fast: {}, quality: { reason: driverTooOld } }),
      targetCompatibility(gpu1, { fast: { reason: driverTooOld }, quality: {} }),
    );
    const { user } = renderCatalog(
      "/models/catalog",
      vi.fn(() => Promise.resolve(read)),
    );

    await user.click(await screen.findByRole("button", { name: "Runs on filter: this site" }));
    const menu = await screen.findByRole("menu");
    expect(
      within(menu)
        .getAllByRole("menuitemcheckbox")
        .map((item) => item.textContent),
    ).toEqual(["this site1", "rig1"]);
    await user.click(within(menu).getByRole("menuitemcheckbox", { name: /^rig/ }));
    await user.keyboard("{Escape}");

    expect(screen.getByRole("button", { name: "Runs on filter: rig" })).toBeVisible();
    expect(
      within(screen.getByRole("row", { name: "Alpha 7B" })).getAllByRole("cell")[0],
    ).toHaveTextContent("Alpha 7BVision · Chat, Decisions · 2 of 5 profiles");
  });

  it("says no model runs on a machine and offers every model", async () => {
    const { router, user } = renderCatalog("/models/catalog?f=runs&runs=node%3Anode-b");

    expect(await screen.findByText(/^No model matches the filter\.$/)).toBeVisible();
    expect(modelRows()).toHaveLength(1);

    await user.click(screen.getByRole("button", { name: "Show all models" }));
    expect(currentLocation(router)).toBe("/models/catalog?f=");
    expect(modelRows()).toEqual(["Alpha 7B", "Bare"]);
  });

  it("lists every model without the chip on a site without model targets, and says why", async () => {
    renderCatalog(
      "/models/catalog",
      vi.fn(() => Promise.resolve(create(ListModelProfileCompatibilityResponseSchema, {}))),
    );

    expect(
      await screen.findByText("Runs on isn't applied: this site has no model execution target."),
    ).toBeVisible();
    expect(modelRows()).toEqual(["Alpha 7B", "Bare"]);
    expect(screen.queryByRole("button", { name: /^Runs on filter/ })).not.toBeInTheDocument();
  });

  it("shows the read's estimates per hardware class while no target reports", async () => {
    const gib = 1024n ** 3n;
    const { user } = renderCatalog(
      "/models/catalog?f=",
      vi.fn(() =>
        Promise.resolve(
          create(ListModelProfileCompatibilityResponseSchema, {
            profiles: [
              profileSizing(
                "alpha-7b",
                "fast",
                { acceleratorMemoryBytes: 8n * gib, hardwareClassId: "nvidia" },
                {
                  acceleratorMemoryBytes: 6n * gib,
                  hardwareClassId: "apple",
                  hostMemoryBytes: 2n * gib,
                },
              ),
            ],
            targets: [notReporting(pool), notReporting(silent)],
          }),
        ),
      ),
    );

    await user.click(await screen.findByRole("button", { name: "Show profiles for Alpha 7B" }));
    await waitFor(() =>
      expect(
        within(screen.getByRole("row", { name: "Alpha 7B · Fast" })).getAllByRole("cell")[2],
      ).toHaveTextContent("6–8 GiB"),
    );

    await user.click(screen.getByRole("row", { name: "Alpha 7B · Fast" }));
    const profile = within(
      await screen.findByRole("complementary", { name: "Alpha 7B · fast" }),
    ).getByRole("region", { name: "Profile" });
    expect(within(profile).getByText("Memory").nextElementSibling).toHaveTextContent(
      "NVIDIA 8 GiBapple 6 GiB + 2 GiB host RAM",
    );
  });

  it("lists every model without the chip when the read fails, and says so", async () => {
    renderCatalog(
      "/models/catalog",
      vi.fn(() => Promise.reject(new Error("unavailable"))),
    );

    expect(
      await screen.findByText("Runs on isn't available: the compatibility read failed."),
    ).toBeVisible();
    expect(modelRows()).toEqual(["Alpha 7B", "Bare"]);
  });
});
