import { create } from "@bufbuild/protobuf";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import type { ModelClient } from "@/api/model-client";
import {
  currentLocation,
  modelTarget,
  renderModelsSection,
} from "@/features/models/deployed/models-section-test-support";
import {
  compatibilityResponse,
  driverTooOld,
  profileSizing,
  targetCompatibility,
} from "@/features/models/profiles/profile-compatibility-test-support";
import { MemoryFit } from "@/gen/dotcore/deployment/v1/memory_fit_pb";
import {
  type ListModelProfileCompatibilityResponse,
  ListModelProfileCompatibilityResponseSchema,
  ModelProfileSizingSchema,
  SizingScenarioUnsupportedReason,
} from "@/gen/dotcore/model/v1/service_pb";
import { productionTiming, setTiming } from "@/lib/timing";
import { useFakeTimersWithWaits } from "@/test/fake-timers";
import { setupUser } from "@/test/user";

import { syntheticCatalog } from "./catalog-table-test-support";

const gib = 1024n ** 3n;

type CompatibilityRequest = Parameters<
  NonNullable<ModelClient["model"]["listModelProfileCompatibility"]>
>[0];

// A site with an NVIDIA workstation ("gpu-box") and a Mac ("mac"). Fast runs on both; Quality runs
// only on the workstation. At defaults Fast and Quality fit the workstation and the Mac's fit for
// Fast is unknown; Fast starts from 400k total context on the workstation and 179k on the Mac, and
// Quality from 256k; under a scenario, Fast starts from 128k. At 32k per request with 128k total, Fast exceeds the workstation but fits the Mac,
// and Quality's llama.cpp can't serve the scenario.
const pool = { executionTargetId: "pool", siteNodeId: "gpu-box" };
const metal = { executionTargetId: "metal", siteNodeId: "mac" };
const notRunning = { reason: driverTooOld };

function siteRead(request: CompatibilityRequest = {}): ListModelProfileCompatibilityResponse {
  const scenario = Boolean(request.scenario);
  const { EXCEEDS, FITS, UNKNOWN, UNSPECIFIED } = MemoryFit;
  const alpha = compatibilityResponse(
    "alpha-7b",
    targetCompatibility(pool, {
      cpu: notRunning,
      fast: {
        startingTotalContextTokens: scenario ? 131072 : 409600,
        memoryFit: scenario ? EXCEEDS : FITS,
      },
      orphan: notRunning,
      quality: scenario
        ? {
            memoryFit: UNSPECIFIED,
            unsupportedReason: SizingScenarioUnsupportedReason.PARAMETER_OUT_OF_RANGE,
          }
        : { startingTotalContextTokens: 262144 },
      sized: notRunning,
    }),
    targetCompatibility(metal, {
      cpu: notRunning,
      fast: {
        hardwareClassId: "apple",
        startingTotalContextTokens: scenario ? 131072 : 183296,
        memoryFit: scenario ? FITS : UNKNOWN,
      },
      orphan: notRunning,
      quality: notRunning,
      sized: notRunning,
    }),
  );
  const bare = compatibilityResponse(
    "bare",
    targetCompatibility(pool, { plain: notRunning }),
    targetCompatibility(metal, { plain: notRunning }),
  );
  return create(ListModelProfileCompatibilityResponseSchema, {
    entries: [...alpha.entries, ...bare.entries],
    profiles: [
      profileSizing(
        "alpha-7b",
        "fast",
        { acceleratorMemoryBytes: (scenario ? 20n : 8n) * gib, hardwareClassId: "nvidia" },
        { acceleratorMemoryBytes: (scenario ? 12n : 6n) * gib, hardwareClassId: "apple" },
      ),
      scenario
        ? {
            ...profileSizing("alpha-7b", "quality"),
            unsupportedReason: SizingScenarioUnsupportedReason.PARAMETER_OUT_OF_RANGE,
          }
        : profileSizing("alpha-7b", "quality", {
            acceleratorMemoryBytes: 9n * gib,
            hardwareClassId: "nvidia",
          }),
    ],
    targets: alpha.targets,
  });
}

function renderCatalog(
  initialEntry: string,
  read: (request: CompatibilityRequest) => Promise<ListModelProfileCompatibilityResponse> = (
    request,
  ) => Promise.resolve(siteRead(request)),
) {
  const { engineReleases, hardwareClasses, models } = syntheticCatalog();
  const compatibility = vi.fn(read);
  return {
    ...renderModelsSection({
      compatibility,
      engineReleases,
      executionTargets: [
        modelTarget("gpu-box", "pool", "GPU", "gpu-box"),
        modelTarget("mac", "metal", "Metal", "mac"),
      ],
      hardwareClasses,
      initialEntry,
      models,
    }),
    compatibility,
    user: setupUser(),
  };
}

function table() {
  return screen.getByRole("table", { name: "Model catalog" });
}

function cells(name: string): string[] {
  return within(screen.getByRole("row", { name }))
    .getAllByRole("cell")
    .map((cell) => cell.textContent);
}

function modelRows(): string[] {
  return within(table())
    .getAllByRole("row")
    .slice(1)
    .map((row) => row.getAttribute("aria-label") ?? row.textContent);
}

async function expandAlpha(user: UserEvent) {
  await user.click(await screen.findByRole("button", { name: "Show profiles for Alpha 7B" }));
  await waitFor(() => expect(screen.queryByText("Loading estimate")).not.toBeInTheDocument());
}

function footer(): string | null {
  return screen.getByText(/^\d+ of \d+ models/).textContent;
}

async function openRunsOn(user: UserEvent) {
  const toolbar = screen.getByRole("region", { name: "Catalog models" });
  await user.click(within(toolbar).getAllByRole("button", { name: /^Runs on filter/ })[0]);
  return screen.findByRole("menu");
}

// A read that waits on `finish` for a scenario, and reads defaults at once.
function heldScenarioRead() {
  const held = { finish: () => undefined as void };
  const read = (request: CompatibilityRequest) =>
    request.scenario
      ? new Promise<ListModelProfileCompatibilityResponse>((resolve) => {
          held.finish = () => resolve(siteRead(request));
        })
      : Promise.resolve(siteRead(request));
  return { held, read };
}

// The catalog's 32k per request with 128k total, as its URL and the read carry it.
const scenarioSearch = "req=32768&ctx=131072";
const scenario = { contextTokensPerRequest: 32768n, totalContextTokens: 131072n };
const sizedFor = "32k per request · 128k total";

describe("ModelCatalogPage Runs on sized for", () => {
  it("restores machines and Sized for from the URL, reads the scenario, and judges any machine", async () => {
    const { compatibility, user } = renderCatalog(
      `/models/catalog?f=runs&runs=node%3Agpu-box&runs=node%3Amac&${scenarioSearch}`,
    );
    // Fast exceeds gpu-box but fits the Mac, so it stays; Quality can't serve the scenario.
    await waitFor(() => expect(footer()).toBe("1 of 2 models · 1 hidden"));

    expect(compatibility).toHaveBeenNthCalledWith(1, { scenario });
    expect(
      screen.getByRole("button", { name: `Runs on filter: gpu-box, mac, ${sizedFor}` }),
    ).toHaveTextContent(`Runs ongpu-box, mac·${sizedFor}`);
    expect(modelRows()).toEqual(["Alpha 7B"]);
    expect(cells("Alpha 7B")[2]).toBe("12–20 GiB");

    // The custom scenario's inputs restore from the URL.
    let menu = await openRunsOn(user);
    expect(within(menu).getByRole("menuitemradio", { name: "Custom" })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    expect(within(menu).getByRole("textbox", { name: "Total context" })).toHaveValue("128k");
    expect(within(menu).getByRole("textbox", { name: "Context per request" })).toHaveValue("32k");
    await user.keyboard("{Escape}");

    // At defaults on gpu-box alone, both profiles fit.
    menu = await openRunsOn(user);
    await user.keyboard("{Shift>}");
    await user.click(within(menu).getByRole("menuitemcheckbox", { name: /mac/ }));
    await user.keyboard("{/Shift}");
    await user.click(within(menu).getByRole("menuitemradio", { name: "Defaults" }));
    await user.keyboard("{Escape}");
    await expandAlpha(user);
    expect(cells("Alpha 7B · Fast")[2]).toBe("8 GiBfits");
    expect(cells("Alpha 7B · Quality")[2]).toBe("9 GiBfits");
  });

  it("shows the starting total context at defaults, and the chosen total under a scenario", async () => {
    const { compatibility, router, user } = renderCatalog("/models/catalog?f=runs&runs=node%3Amac");
    // On the Mac alone, Alpha shows Fast alone: the model's maximum context and the total context
    // Fast's launch starts from there, labeled as the wizard's hint labels it.
    await waitFor(() => expect(modelRows()).toEqual(["Alpha 7B"]));
    await waitFor(() => expect(cells("Alpha 7B").slice(3)).toEqual(["256k", "179k"]));

    // Over the site, the largest over the machines that run each profile.
    await act(() => router.navigate("/models/catalog"));
    await expandAlpha(user);
    expect(cells("Alpha 7B · Fast").slice(3)).toEqual(["256k", "400k"]);
    expect(cells("Alpha 7B · Quality").slice(3)).toEqual(["256k", "256k"]);
    expect(cells("Alpha 7B").slice(3)).toEqual(["256k", "256k–400k"]);

    // A custom context per request sizes every profile, and the read carries it alone; the table
    // shows the total each profile starts from under it.
    const menu = await openRunsOn(user);
    await user.click(within(menu).getByRole("menuitemradio", { name: "Custom" }));
    await user.keyboard("{Escape}");
    await waitFor(() => expect(currentLocation(router)).toBe("/models/catalog?req=32768"));
    await waitFor(() =>
      expect(compatibility).toHaveBeenLastCalledWith({
        scenario: { contextTokensPerRequest: 32768n },
      }),
    );
    // Quality can't serve it, so Alpha shows Fast alone.
    await waitFor(() => expect(footer()).toBe("1 of 2 models · 1 hidden"));
    expect(cells("Alpha 7B").slice(3)).toEqual(["256k", "128k"]);

    // Removing the chip drops the scenario and restores the defaults over every profile.
    await user.click(screen.getByRole("button", { name: "Remove Runs on filter" }));
    await waitFor(() => expect(currentLocation(router)).toBe("/models/catalog?f="));
    await waitFor(() => expect(compatibility).toHaveBeenLastCalledWith({}));
    await waitFor(() => expect(cells("Alpha 7B").slice(3)).toEqual(["64k–256k", "256k–400k"]));
  });

  it("keeps a profile whose fit is unknown on a machine, and spans the site without the chip", async () => {
    const { user } = renderCatalog("/models/catalog?f=runs&runs=node%3Amac");
    await waitFor(() => expect(modelRows()).toEqual(["Alpha 7B"]));
    await waitFor(() => expect(cells("Alpha 7B")[2]).toBe("6 GiB"));
    expect(footer()).toBe("1 of 2 models");

    await user.click(screen.getByRole("button", { name: "Remove Runs on filter" }));
    await expandAlpha(user);
    expect(cells("Alpha 7B · Fast")[2]).toBe("6–8 GiB");
  });

  it("says nothing runs sized for the scenario, counts why, and shows every model at defaults once removed", async () => {
    const { compatibility, router, user } = renderCatalog(
      `/models/catalog?f=runs&runs=node%3Agpu-box&${scenarioSearch}`,
    );

    await waitFor(() => expect(footer()).toBe("0 of 2 models · 2 hidden"));
    expect(
      within(table()).getByText("No model matches the filter.", {
        exact: false,
      }),
    ).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Show all models" }));
    await waitFor(() => expect(currentLocation(router)).toBe("/models/catalog?f="));
    await expandAlpha(user);
    expect(cells("Alpha 7B · Fast")[2]).toBe("6–8 GiB");
    expect(compatibility).toHaveBeenLastCalledWith({});
  });

  it("hides a profile too large at defaults", async () => {
    const { user } = renderCatalog("/models/catalog?f=runs&runs=node%3Amac", (request) => {
      const response = siteRead(request);
      const fast = response.entries.find(
        ({ profileId, target }) => profileId === "fast" && target?.siteNodeId === "mac",
      )!;
      fast.memoryFit = MemoryFit.EXCEEDS;
      fast.freeMemoryFit = MemoryFit.EXCEEDS;
      return Promise.resolve(response);
    });

    await waitFor(() => expect(footer()).toBe("0 of 2 models · 1 hidden"));
    expect(
      within(table()).getByText("No model matches the filter.", {
        exact: false,
      }),
    ).toBeVisible();

    // Without the chip, nothing is judged against memory.
    await user.click(screen.getByRole("button", { name: "Remove Runs on filter" }));
    expect(modelRows()).toEqual(["Alpha 7B", "Bare"]);
    expect(footer()).toBe("2 of 2 models");
  });

  it("keeps the previous verdicts while a new scenario's read is pending", async () => {
    const { held, read } = heldScenarioRead();
    const { router, user } = renderCatalog("/models/catalog?f=runs&runs=node%3Agpu-box", read);
    await expandAlpha(user);
    expect(modelRows()).toEqual(["Alpha 7B", "Alpha 7B · Quality", "Alpha 7B · Fast"]);

    const menu = await openRunsOn(user);
    await user.click(within(menu).getByRole("menuitemradio", { name: "Custom" }));
    await user.keyboard("{Escape}");
    expect(currentLocation(router)).toBe("/models/catalog?f=runs&runs=node%3Agpu-box&req=32768");

    // Neither widened nor narrowed until the scenario is sized.
    await waitFor(() => expect(cells("Alpha 7B · Fast")[2]).toBe("Loading estimate"));
    expect(modelRows()).toEqual(["Alpha 7B", "Alpha 7B · Quality", "Alpha 7B · Fast"]);

    act(() => held.finish());
    expect(
      await within(table()).findByText("No model matches the filter.", {
        exact: false,
      }),
    ).toBeVisible();
  });

  it("spans the scenario sliders over every profile's declared ends", async () => {
    const { user } = renderCatalog("/models/catalog?f=runs&runs=node%3Amac");
    await waitFor(() => expect(modelRows()).toEqual(["Alpha 7B"]));

    const menu = await openRunsOn(user);
    await user.click(within(menu).getByRole("menuitemradio", { name: "Custom" }));
    // The vLLM configuration stops at 64k, but the llama.cpp ones reach the 256k model maximum, one
    // request at a time; the scale starts at the declared 1024 minimum.
    const total = within(menu).getByRole("slider", { name: "Total context" });
    const perRequest = within(menu).getByRole("slider", { name: "Context per request" });
    expect(perRequest).toHaveValue("625");
    expect(
      within(within(menu).getByRole("group", { name: "Total context presets" }))
        .getAllByRole("button")
        .map((chip) => chip.textContent),
    ).toEqual(["8k", "32k", "64k", "128k", "256k"]);

    fireEvent.change(perRequest, { target: { value: "1000" } });
    expect(within(menu).getByRole("textbox", { name: "Context per request" })).toHaveValue("256k");
    fireEvent.change(total, { target: { value: "1000" } });
    expect(within(menu).getByRole("textbox", { name: "Total context" })).toHaveValue("256k");
  });

  it("applies typed values once typing pauses, presets at once, and defaults again", async () => {
    const { compatibility, router, user } = renderCatalog("/models/catalog?f=runs&runs=node%3Amac");
    await waitFor(() => expect(cells("Alpha 7B")[2]).toBe("6 GiB"));

    let menu = await openRunsOn(user);
    expect(within(menu).getByRole("menuitemradio", { name: "Defaults" })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    expect(within(menu).queryByRole("textbox")).not.toBeInTheDocument();
    await user.click(within(menu).getByRole("menuitemradio", { name: "Custom" }));
    await waitFor(() =>
      expect(currentLocation(router)).toBe("/models/catalog?f=runs&runs=node%3Amac&req=32768"),
    );
    expect(within(menu).getByRole("slider", { name: "Context per request" })).toBeVisible();
    expect(within(menu).getByRole("slider", { name: "Total context" })).toBeVisible();
    // Empty parallel requests leave each profile its own, and an empty total context follows.
    const parallel = within(menu).getByRole("textbox", { name: "Parallel requests" });
    expect(parallel).toHaveValue("");
    expect(parallel).toHaveAttribute("placeholder", "Profile default");
    const total = within(menu).getByRole("textbox", { name: "Total context" });
    expect(total).toHaveValue("");
    expect(total).not.toHaveAttribute("placeholder");
    await user.type(parallel, "2");
    expect(total).toHaveAttribute("placeholder", "64k");
    await waitFor(() =>
      expect(currentLocation(router)).toBe(
        "/models/catalog?f=runs&runs=node%3Amac&req=32768&par=2",
      ),
    );

    await user.click(
      within(within(menu).getByRole("group", { name: "Total context presets" })).getByRole(
        "button",
        { name: "256k" },
      ),
    );
    await waitFor(() =>
      expect(currentLocation(router)).toBe(
        "/models/catalog?f=runs&runs=node%3Amac&req=32768&par=2&ctx=262144",
      ),
    );
    await waitFor(() =>
      expect(compatibility).toHaveBeenLastCalledWith({
        scenario: {
          contextTokensPerRequest: 32768n,
          parallelRequests: 2,
          totalContextTokens: 262144n,
        },
      }),
    );
    await user.keyboard("{Escape}");
    expect(
      screen.getByRole("button", {
        name: "Runs on filter: mac, 32k per request · 2 parallel · 256k total",
      }),
    ).toBeVisible();

    menu = await openRunsOn(user);
    await user.click(within(menu).getByRole("menuitemradio", { name: "Defaults" }));
    expect(currentLocation(router)).toBe("/models/catalog?f=runs&runs=node%3Amac");
    await waitFor(() => expect(cells("Alpha 7B")[2]).toBe("6 GiB"));
  });

  it("sets a scenario with the inspector open in one navigation, keeping the panel and its history", async () => {
    const { router, user } = renderCatalog("/models/catalog");
    await user.click(await screen.findByRole("row", { name: "Alpha 7B" }));
    await screen.findByRole("complementary", { name: "Alpha 7B" });
    const panelState: unknown = router.state.location.state;
    expect(panelState).toBeTruthy();
    const navigations: string[] = [];
    const unsubscribe = router.subscribe((state) =>
      navigations.push(state.location.pathname + state.location.search),
    );

    const menu = await openRunsOn(user);
    await user.click(within(menu).getByRole("menuitemradio", { name: "Custom" }));
    await waitFor(() =>
      expect(currentLocation(router)).toBe(
        "/models/catalog?req=32768&panel=model-catalog%2Fcatalog%3Aalpha-7b",
      ),
    );
    await new Promise((resolve) => setTimeout(resolve, 50));
    unsubscribe();

    expect([...new Set(navigations)]).toEqual([
      "/models/catalog?req=32768&panel=model-catalog%2Fcatalog%3Aalpha-7b",
    ]);
    expect(router.state.location.state).toEqual(panelState);
    expect(screen.getByRole("complementary", { name: "Alpha 7B" })).toBeVisible();
  });

  it("judges support alone and says so when the scenario's read fails", async () => {
    const { user } = renderCatalog("/models/catalog?f=runs&runs=node%3Agpu-box", (request) =>
      request.scenario
        ? Promise.reject(new Error("unavailable"))
        : Promise.resolve(siteRead(request)),
    );
    await expandAlpha(user);

    const menu = await openRunsOn(user);
    await user.click(within(menu).getByRole("menuitemradio", { name: "Custom" }));
    await user.keyboard("{Escape}");

    expect(
      await screen.findByText("Runs on doesn't check memory: the sizing read failed."),
    ).toBeVisible();
    expect(new Set(modelRows())).toEqual(
      new Set(["Alpha 7B", "Alpha 7B · Quality", "Alpha 7B · Fast"]),
    );
  });

  it("marks a class the scenario can't serve instead of another class's estimate standing in", async () => {
    // Under the scenario Fast fits gpu-box but can't be sized on the Mac's apple class.
    const read = (request: CompatibilityRequest) => {
      const response = siteRead(request);
      if (!request.scenario) return Promise.resolve(response);
      const fast = response.profiles.find(({ profileId }) => profileId === "fast")!;
      fast.hardwareClassEstimates = fast.hardwareClassEstimates.filter(
        ({ hardwareClassId }) => hardwareClassId === "nvidia",
      );
      fast.unsupportedHardwareClassIds = ["apple"];
      // Quality serves the scenario here, so Alpha keeps two profiles.
      const quality = response.profiles.findIndex(({ profileId }) => profileId === "quality");
      response.profiles[quality] = create(
        ModelProfileSizingSchema,
        profileSizing("alpha-7b", "quality", {
          acceleratorMemoryBytes: 9n * gib,
          hardwareClassId: "nvidia",
        }),
      );
      for (const entry of response.entries.filter(({ profileId }) =>
        ["fast", "quality"].includes(profileId),
      )) {
        if (entry.target?.siteNodeId === "mac") {
          if (entry.profileId === "quality") continue;
          entry.memoryFit = MemoryFit.UNSPECIFIED;
          entry.freeMemoryFit = MemoryFit.UNSPECIFIED;
          entry.unsupportedReason = SizingScenarioUnsupportedReason.DERIVED_PARAMETER_OUT_OF_RANGE;
        } else {
          entry.memoryFit = MemoryFit.FITS;
          entry.freeMemoryFit = MemoryFit.FITS;
          entry.unsupportedReason = SizingScenarioUnsupportedReason.UNSPECIFIED;
        }
      }
      return Promise.resolve(response);
    };
    const { router, user } = renderCatalog(`/models/catalog?${scenarioSearch}`, read);
    await expandAlpha(user);
    expect(cells("Alpha 7B · Fast")[2]).toBe("20 GiB");

    await user.click(screen.getByRole("row", { name: "Alpha 7B · Fast" }));
    const profile = within(
      await screen.findByRole("complementary", { name: "Alpha 7B · fast" }),
    ).getByRole("region", { name: "Profile" });
    expect(within(profile).getByText("Memory").nextElementSibling).toHaveTextContent(
      "NVIDIA 20 GiBapple unsupported for this scenario",
    );

    // On the Mac alone Fast can't serve the scenario, so Runs on hides it and counts it.
    await act(() => router.navigate(`/models/catalog?f=runs&runs=node%3Amac&${scenarioSearch}`));
    await waitFor(() => expect(footer()).toBe("0 of 2 models · 1 hidden"));
  });

  it("flags a context per request above a chosen total once left, and doesn't apply it", async () => {
    const { compatibility, router, user } = renderCatalog("/models/catalog?f=runs&runs=node%3Amac");
    await waitFor(() => expect(modelRows()).toEqual(["Alpha 7B"]));

    const menu = await openRunsOn(user);
    await user.click(within(menu).getByRole("menuitemradio", { name: "Custom" }));
    const total = within(menu).getByRole("textbox", { name: "Total context" });
    const perRequest = within(menu).getByRole("textbox", { name: "Context per request" });
    await user.click(
      within(within(menu).getByRole("group", { name: "Total context presets" })).getByRole(
        "button",
        { name: "64k" },
      ),
    );
    await waitFor(() =>
      expect(currentLocation(router)).toBe(
        "/models/catalog?f=runs&runs=node%3Amac&req=32768&ctx=65536",
      ),
    );

    // The scenario's read lands before the next edit.
    await waitFor(() =>
      expect(compatibility).toHaveBeenLastCalledWith({
        scenario: { contextTokensPerRequest: 32768n, totalContextTokens: 65536n },
      }),
    );
    await act(async () => {
      await Promise.resolve();
    });

    // A chip sets the value at once, so no smaller value typed on the way applies.
    await user.click(
      within(within(menu).getByRole("group", { name: "Context per request presets" })).getByRole(
        "button",
        { name: "256k" },
      ),
    );
    await user.click(perRequest);
    await user.click(total);
    expect(
      await screen.findByText("Context per request must be at most the total context."),
    ).toBeVisible();
    expect(perRequest).toHaveAttribute("aria-invalid", "true");
    expect(currentLocation(router)).toBe(
      "/models/catalog?f=runs&runs=node%3Amac&req=32768&ctx=65536",
    );
  });

  it("flags a total that isn't a whole number once left, leaves the context per request, and doesn't apply it", async () => {
    const { compatibility, router, user } = renderCatalog("/models/catalog?f=runs&runs=node%3Amac");
    await waitFor(() => expect(modelRows()).toEqual(["Alpha 7B"]));

    const menu = await openRunsOn(user);
    await user.click(within(menu).getByRole("menuitemradio", { name: "Custom" }));
    await waitFor(() =>
      expect(currentLocation(router)).toBe("/models/catalog?f=runs&runs=node%3Amac&req=32768"),
    );
    const total = within(menu).getByRole("textbox", { name: "Total context" });
    const perRequest = within(menu).getByRole("textbox", { name: "Context per request" });
    await user.clear(perRequest);
    await user.type(perRequest, "64k", { skipClick: true });
    // The total field takes focus from the click, so typing doesn't click it again.
    await user.click(total);
    await user.type(total, "1.3k", { skipClick: true });
    await user.click(perRequest);
    expect(screen.getByText("Total context must be a whole number of tokens.")).toBeVisible();
    expect(total).toHaveAttribute("aria-invalid", "true");
    expect(perRequest).toHaveValue("64k");
    // Leaving the context per request applied it; the invalid total never applies.
    await waitFor(() =>
      expect(currentLocation(router)).toBe("/models/catalog?f=runs&runs=node%3Amac&req=65536"),
    );
    await waitFor(() =>
      expect(
        new Set(
          compatibility.mock.calls.flatMap(([request]) =>
            request.scenario
              ? [
                  `${request.scenario.contextTokensPerRequest}/${request.scenario.parallelRequests ?? ""}/${request.scenario.totalContextTokens ?? ""}`,
                ]
              : [],
          ),
        ),
      ).toEqual(new Set(["32768//", "65536//"])),
    );
  });

  it("shows Reset for a changed Sized for, and restores the chips and the defaults in one navigation", async () => {
    const { compatibility, router, user } = renderCatalog("/models/catalog?f=runs&runs=node%3Amac");
    const toolbar = await screen.findByRole("region", { name: "Catalog models" });
    await user.click(within(toolbar).getByRole("button", { name: "Reset" }));
    await waitFor(() => expect(currentLocation(router)).toBe("/models/catalog"));
    expect(within(toolbar).queryByRole("button", { name: "Reset" })).not.toBeInTheDocument();

    // Sized for alone shows Reset.
    const menu = await openRunsOn(user);
    await user.click(within(menu).getByRole("menuitemradio", { name: "Custom" }));
    await user.keyboard("{Escape}");
    await waitFor(() => expect(currentLocation(router)).toBe("/models/catalog?req=32768"));
    await user.click(within(toolbar).getByRole("button", { name: "Reset" }));
    await waitFor(() => expect(currentLocation(router)).toBe("/models/catalog"));
    expect(within(toolbar).queryByRole("button", { name: "Reset" })).not.toBeInTheDocument();
    await waitFor(() => expect(compatibility).toHaveBeenLastCalledWith({}));

    // Changed chips and Sized for reset together.
    await act(() => router.navigate(`/models/catalog?f=runs&runs=node%3Amac&${scenarioSearch}`));
    await user.click(await within(toolbar).findByRole("button", { name: "Reset" }));
    await waitFor(() => expect(currentLocation(router)).toBe("/models/catalog"));
  });

  it("drops a scenario without the Runs on chip from the URL and reads defaults", async () => {
    const { compatibility, router } = renderCatalog(`/models/catalog?f=&${scenarioSearch}`);

    await waitFor(() => expect(currentLocation(router)).toBe("/models/catalog?f="));
    await waitFor(() => expect(modelRows()).toEqual(["Alpha 7B", "Bare"]));
    expect(compatibility.mock.calls.every(([request]) => !request.scenario)).toBe(true);
  });

  it("removes the chip and its scenario when Remove filter follows typing at once", async () => {
    setTiming(productionTiming);
    useFakeTimersWithWaits();
    const { router, user } = renderCatalog("/models/catalog?f=runs&runs=node%3Amac");
    await waitFor(() => expect(modelRows()).toEqual(["Alpha 7B"]));

    const menu = await openRunsOn(user);
    await user.click(within(menu).getByRole("menuitemradio", { name: "Custom" }));
    await user.type(within(menu).getByRole("textbox", { name: "Context per request" }), "2048");
    await user.click(within(menu).getByRole("menuitem", { name: "Remove filter" }));

    await waitFor(() => expect(currentLocation(router)).toBe("/models/catalog?f="));
    await act(() => vi.advanceTimersByTimeAsync(productionTiming.scenarioInputMs * 2));
    expect(currentLocation(router)).toBe("/models/catalog?f=");
    expect(screen.queryByRole("button", { name: /^Runs on filter/ })).not.toBeInTheDocument();
  });

  it("drops a scenario on a site that doesn't offer Runs on", async () => {
    const { compatibility, router } = renderCatalog(`/models/catalog?${scenarioSearch}`, () =>
      Promise.resolve(create(ListModelProfileCompatibilityResponseSchema, {})),
    );

    await waitFor(() => expect(currentLocation(router)).toBe("/models/catalog"));
    await waitFor(() => expect(compatibility).toHaveBeenLastCalledWith({}));
  });
});

describe("ModelCatalogPage Runs on memory basis", () => {
  // At defaults Fast fits the workstation's total but not its free memory.
  const freeExceeds = (request: CompatibilityRequest) => {
    const response = siteRead(request);
    response.entries.find(
      ({ profileId, target }) => profileId === "fast" && target?.siteNodeId === "gpu-box",
    )!.freeMemoryFit = MemoryFit.EXCEEDS;
    return Promise.resolve(response);
  };
  const onGpuBox = "/models/catalog?f=runs&runs=node%3Agpu-box";

  function panelRows(): string[] {
    return within(screen.getByRole("table", { name: "Model profiles" }))
      .getAllByRole("row")
      .slice(1)
      .map((row) => row.getAttribute("aria-label") ?? row.textContent);
  }

  it("starts on free now, keeps Total in the URL across a reload, and Reset clears it", async () => {
    const { router, user } = renderCatalog(onGpuBox, freeExceeds);
    await expandAlpha(user);
    expect(screen.queryByRole("row", { name: "Alpha 7B · Fast" })).not.toBeInTheDocument();
    expect(footer()).toBe("1 of 2 models · 1 hidden");
    expect(screen.getByRole("button", { name: "Runs on filter: gpu-box" })).toBeVisible();

    const menu = await openRunsOn(user);
    expect(within(menu).getByRole("menuitemradio", { name: "Free now" })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await user.click(within(menu).getByRole("menuitemradio", { name: "Total" }));
    await user.keyboard("{Escape}");
    await waitFor(() => expect(currentLocation(router)).toBe(`${onGpuBox}&mem=total`));
    expect(
      screen.getByRole("button", { name: "Runs on filter: gpu-box, Total memory" }),
    ).toBeVisible();
    expect(screen.getByRole("row", { name: "Alpha 7B · Fast" })).toBeVisible();
    expect(footer()).toBe("1 of 2 models");
    cleanup();

    // A reload keeps Total.
    const reloaded = renderCatalog(`${onGpuBox}&mem=total`, freeExceeds);
    await expandAlpha(reloaded.user);
    expect(screen.getByRole("row", { name: "Alpha 7B · Fast" })).toBeVisible();
    expect(footer()).toBe("1 of 2 models");

    const toolbar = screen.getByRole("region", { name: "Catalog models" });
    await reloaded.user.click(within(toolbar).getByRole("button", { name: "Reset" }));
    await waitFor(() => expect(currentLocation(reloaded.router)).toBe("/models/catalog"));
    expect(screen.getByRole("button", { name: "Runs on filter: this site" })).toBeVisible();
  });

  it("drops Total from the URL with the Runs on chip", async () => {
    const { router } = renderCatalog("/models/catalog?f=&mem=total", freeExceeds);

    await waitFor(() => expect(currentLocation(router)).toBe("/models/catalog?f="));
  });

  it("opens the panel and the wizard on the catalog's Total, and the panel's switch leaves the catalog", async () => {
    const search = `${onGpuBox}&mem=total&panel=model-catalog%2Fcatalog%3Aalpha-7b`;
    const { router, user } = renderCatalog(search, freeExceeds);
    const profiles = await screen.findByRole("region", { name: "Profiles" });
    const totalChip = "Runs on filter: gpu-box, Total memory";
    expect(await within(profiles).findByRole("button", { name: totalChip })).toBeVisible();
    await waitFor(() => expect(panelRows()).toEqual(["Quality", "Fast"]));

    // Free now inside the panel hides Fast there and leaves the catalog on Total.
    await user.click(within(profiles).getByRole("button", { name: totalChip }));
    const menu = await screen.findByRole("menu");
    await user.click(within(menu).getByRole("menuitemradio", { name: "Free now" }));
    await user.keyboard("{Escape}");
    expect(within(profiles).getByRole("button", { name: "Runs on filter: gpu-box" })).toBeVisible();
    expect(panelRows()).toEqual(["Quality"]);
    expect(screen.getByText(/^\d+ of \d+ profiles?/)).toHaveTextContent(
      "1 of 5 profiles · 1 hidden",
    );
    expect(currentLocation(router)).toBe(search);

    // Reset returns the panel to the catalog's Total.
    await user.click(within(profiles).getByRole("button", { name: "Reset" }));
    expect(await within(profiles).findByRole("button", { name: totalChip })).toBeVisible();

    // The wizard starts on the panel's Total.
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    const dialog = await screen.findByRole("dialog", { name: /Deploy model/ });
    expect(await within(dialog).findByRole("button", { name: totalChip })).toBeVisible();
    expect(currentLocation(router)).toBe(search);
  });
});

describe("ModelCatalogPage model panel Runs on", () => {
  function panelRows(): string[] {
    return within(screen.getByRole("table", { name: "Model profiles" }))
      .getAllByRole("row")
      .slice(1)
      .map((row) => row.getAttribute("aria-label") ?? row.textContent);
  }

  function panelFooter(): string | null {
    return screen.getByText(/^\d+ of \d+ profiles?/).textContent;
  }

  it("applies the catalog's rule under its own Sized for, keeping the shown verdicts while that read is pending", async () => {
    setTiming(productionTiming);
    useFakeTimersWithWaits();
    const { held, read } = heldScenarioRead();
    const search =
      "/models/catalog?f=runs&runs=node%3Agpu-box&panel=model-catalog%2Fcatalog%3Aalpha-7b";
    const { router, user } = renderCatalog(search, read);
    await waitFor(() => expect(panelRows()).toEqual(["Quality", "Fast"]));
    expect(panelFooter()).toBe("2 of 5 profiles");

    const profiles = screen.getByRole("region", { name: "Profiles" });
    await user.click(within(profiles).getByRole("button", { name: "Runs on filter: gpu-box" }));
    const menu = await screen.findByRole("menu");
    await user.click(within(menu).getByRole("menuitemradio", { name: "Custom" }));
    await user.keyboard("{Escape}");
    expect(
      within(profiles).getByRole("button", { name: "Runs on filter: gpu-box, 32k per request" }),
    ).toBeVisible();
    expect(panelRows()).toEqual(["Quality", "Fast"]);
    expect(panelFooter()).toBe("2 of 5 profiles");
    // The catalog keeps its own Sized for.
    expect(currentLocation(router)).toBe(search);

    // The rows keep their order once the loading look shows.
    await act(() => vi.advanceTimersByTimeAsync(productionTiming.loadingRevealMs));
    expect(panelRows()).toEqual(["Quality", "Fast"]);

    act(() => held.finish());
    // On the workstation, Fast exceeds the scenario and Quality can't serve it.
    await waitFor(() =>
      expect(panelRows()).toEqual([
        "No profile runs hereNeeds NVIDIA driver 575 or later (has 560)Show all profiles",
      ]),
    );
    expect(panelFooter()).toBe("0 of 5 profiles · 2 hidden");

    // Deploy starts from the panel's Sized for, not the catalog's.
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    const dialog = await screen.findByRole("dialog", { name: /Deploy model/ });
    act(() => held.finish());
    expect(
      await within(dialog).findByRole("button", {
        name: "Runs on filter: gpu-box, 32k per request",
      }),
    ).toBeVisible();
  });

  it("drops its Sized for with its Runs on chip, leaving the catalog's, and Reset restores both", async () => {
    const search = `/models/catalog?f=runs&runs=node%3Agpu-box&${scenarioSearch}&panel=model-catalog%2Fcatalog%3Aalpha-7b`;
    const { router, user } = renderCatalog(search);
    const profiles = await screen.findByRole("region", { name: "Profiles" });
    const chip = `Runs on filter: gpu-box, ${sizedFor}`;
    expect(await within(profiles).findByRole("button", { name: chip })).toBeVisible();

    await user.click(within(profiles).getByRole("button", { name: "Remove Runs on filter" }));
    // Without the chip, the panel shows every profile at its defaults.
    await waitFor(() => expect(panelFooter()).toBe("5 of 5 profiles"));

    await user.click(within(profiles).getByRole("button", { name: "Filter" }));
    await user.click(await screen.findByRole("menuitem", { name: "Runs on" }));
    await user.keyboard("{Escape}");
    expect(
      await within(profiles).findByRole("button", { name: /^Runs on filter/ }),
    ).not.toHaveAccessibleName(new RegExp(sizedFor));

    await user.click(within(profiles).getByRole("button", { name: "Reset" }));
    expect(await within(profiles).findByRole("button", { name: chip })).toBeVisible();
    expect(within(profiles).queryByRole("button", { name: "Reset" })).not.toBeInTheDocument();
    expect(currentLocation(router)).toBe(search);
  });

  it("shows Reset once its Sized for leaves the catalog's, and restores the catalog's", async () => {
    const search = `/models/catalog?f=runs&runs=node%3Agpu-box&${scenarioSearch}&panel=model-catalog%2Fcatalog%3Aalpha-7b`;
    const { router, user } = renderCatalog(search);
    const profiles = await screen.findByRole("region", { name: "Profiles" });
    const chip = `Runs on filter: gpu-box, ${sizedFor}`;
    expect(await within(profiles).findByRole("button", { name: chip })).toBeVisible();
    expect(within(profiles).queryByRole("button", { name: "Reset" })).not.toBeInTheDocument();

    await user.click(within(profiles).getByRole("button", { name: chip }));
    const menu = await screen.findByRole("menu");
    await user.click(within(menu).getByRole("menuitemradio", { name: "Defaults" }));
    await user.keyboard("{Escape}");
    await user.click(within(profiles).getByRole("button", { name: "Reset" }));

    expect(await within(profiles).findByRole("button", { name: chip })).toBeVisible();
    expect(within(profiles).queryByRole("button", { name: "Reset" })).not.toBeInTheDocument();
    expect(currentLocation(router)).toBe(search);
  });
});
