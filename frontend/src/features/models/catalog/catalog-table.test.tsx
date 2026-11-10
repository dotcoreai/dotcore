import { create } from "@bufbuild/protobuf";
import { act, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import {
  currentLocation,
  renderModelsSection,
} from "@/features/models/deployed/models-section-test-support";
import { profileSizing } from "@/features/models/profiles/profile-compatibility-test-support";
import {
  type ListModelProfileCompatibilityResponse,
  ListModelProfileCompatibilityResponseSchema,
} from "@/gen/dotcore/model/v1/service_pb";
import { setupUser } from "@/test/user";

import { syntheticCatalog } from "./catalog-table-test-support";

// The compatibility read of a site without model targets, sizing the fast and quality profiles of
// the synthetic catalog; the others keep their declared minimum or system RAM.
function siteRead(): ListModelProfileCompatibilityResponse {
  return create(ListModelProfileCompatibilityResponseSchema, {
    profiles: [
      profileSizing("alpha-7b", "fast", {
        acceleratorMemoryBytes: 7_500_000_000n,
        hardwareClassId: "nvidia",
      }),
      profileSizing("alpha-7b", "quality", {
        acceleratorMemoryBytes: 9_000_000_000n,
        hardwareClassId: "nvidia",
      }),
      profileSizing("alpha-7b", "sized"),
      profileSizing("alpha-7b", "cpu"),
      profileSizing("alpha-7b", "orphan"),
      profileSizing("bare", "plain"),
    ],
  });
}

function renderCatalog(initialEntry = "/models/catalog") {
  const user = setupUser();
  const { engineReleases, hardwareClasses, models } = syntheticCatalog();
  const rendered = renderModelsSection({
    compatibility: vi.fn(() => Promise.resolve(siteRead())),
    engineReleases,
    estimate: vi.fn(),
    hardwareClasses,
    initialEntry,
    models,
  });
  return { ...rendered, user };
}

function table() {
  return screen.getByRole("table", { name: "Model catalog" });
}

function headers(): string[] {
  return within(table())
    .getAllByRole("columnheader")
    .map((header) => header.textContent);
}

// Model and profile rows by name, plus the text of any other body row.
function bodyRows(): string[] {
  return within(table())
    .getAllByRole("row")
    .slice(1)
    .map((row) => row.getAttribute("aria-label") ?? row.textContent);
}

function cells(name: string): string[] {
  return within(screen.getByRole("row", { name }))
    .getAllByRole("cell")
    .map((cell) => cell.textContent);
}

function footer() {
  return screen.getByText(/ of \d+ models?$/);
}

async function loaded() {
  await screen.findByRole("table", { name: "Model catalog" });
  await waitFor(() => expect(screen.queryByText("Loading estimate")).not.toBeInTheDocument());
}

describe("ModelCatalogPage table", () => {
  it("shows the default view: Model, Intelligence, Memory, Maximum context, and Total context by intelligence, without Reset", async () => {
    const { client, router } = renderCatalog();
    await loaded();

    expect(headers()).toEqual([
      "Model",
      "Intelligence",
      "Memory",
      "Maximum context",
      "Total context",
    ]);
    expect(
      screen.getByRole("button", { name: "Intelligence column options" }).closest("th"),
    ).toHaveAttribute("aria-sort", "descending");
    expect(
      screen.getByRole("button", { name: "Model column options" }).closest("th"),
    ).not.toHaveAttribute("aria-sort");
    expect(bodyRows()).toEqual(["Alpha 7B", "Bare"]);
    expect(cells("Alpha 7B")).toEqual([
      "Alpha 7BVision · Chat, Decisions · 5 profiles",
      "41.5",
      "5.6–22.4 GiB",
      "64k–256k",
      "—",
    ]);
    expect(cells("Bare")).toEqual(["BareChat · 1 profile", "", "—", "—", "—"]);
    expect(footer()).toHaveTextContent("2 of 2 models");
    expect(screen.queryByRole("button", { name: /^Reset/ })).not.toBeInTheDocument();
    expect(currentLocation(router)).toBe("/models/catalog");
    // Memory comes from the compatibility read alone, never from one estimate per profile.
    expect(client.model.listModelProfileCompatibility).toHaveBeenNthCalledWith(1, {});
    expect(client.model.estimateLocalModelResources).not.toHaveBeenCalled();
  });

  it("shows pending memory until the compatibility read loads, then its values and labels", async () => {
    let finish: () => void = () => undefined;
    const read = new Promise<ListModelProfileCompatibilityResponse>((resolve) => {
      finish = () => resolve(siteRead());
    });
    const user = setupUser();
    const { engineReleases, hardwareClasses, models } = syntheticCatalog();
    const { client } = renderModelsSection({
      compatibility: vi.fn(() => read),
      engineReleases,
      estimate: vi.fn(),
      hardwareClasses,
      initialEntry: "/models/catalog",
      models,
    });

    await user.click(await screen.findByRole("button", { name: "Show profiles for Alpha 7B" }));
    expect(cells("Alpha 7B")[2]).toBe("Loading estimate");
    expect(cells("Alpha 7B · Fast")[2]).toBe("Loading estimate");
    expect(cells("Bare")[2]).toBe("Loading estimate");

    await act(async () => {
      finish();
      await Promise.resolve();
    });

    await loaded();
    expect(cells("Alpha 7B")[2]).toBe("5.6–22.4 GiB");
    expect(bodyRows()).toEqual([
      "Alpha 7B",
      "Alpha 7B · Sized",
      "Alpha 7B · cpu",
      "Alpha 7B · Quality",
      "Alpha 7B · Fast",
      "Alpha 7B · Orphan",
      "Bare",
    ]);
    expect(cells("Alpha 7B · Fast")).toEqual([
      "Fastllama.cppMTPNVIDIAQ4_K_M",
      "",
      "7 GiB",
      "256k",
      "—",
    ]);
    expect(cells("Alpha 7B · Quality")[2]).toBe("8.4 GiB");
    expect(cells("Alpha 7B · Sized")[2]).toBe("≥22.4 GiB");
    expect(cells("Alpha 7B · cpu")[2]).toBe("14.9 GiBsystem RAM");
    expect(cells("Alpha 7B · Orphan").slice(2)).toEqual(["≥5.6 GiB", "256k", "—"]);
    expect(client.model.estimateLocalModelResources).not.toHaveBeenCalled();
  });

  it("searches model names and ids", async () => {
    const { router, user } = renderCatalog();
    await loaded();

    await user.type(screen.getByRole("searchbox", { name: "Filter models" }), "BARE");
    expect(bodyRows()).toEqual(["Bare"]);
    expect(footer()).toHaveTextContent("1 of 2 models");
    expect(currentLocation(router)).toBe("/models/catalog?q=BARE");

    await user.clear(screen.getByRole("searchbox", { name: "Filter models" }));
    await user.type(screen.getByRole("searchbox", { name: "Filter models" }), "alpha-7");
    expect(bodyRows()).toEqual(["Alpha 7B"]);
    expect(screen.getByRole("button", { name: /^Reset/ })).toBeVisible();
  });

  it("adds Engine, chooses a value from counted values, and removes the chip", async () => {
    const { router, user } = renderCatalog();
    await loaded();

    await user.click(screen.getByRole("button", { name: "Filter" }));
    await user.click(await screen.findByRole("menuitem", { name: "Engine" }));
    const menu = await screen.findByRole("menu");
    expect(
      within(menu)
        .getAllByRole("menuitemcheckbox")
        .map((item) => item.textContent),
    ).toEqual(["llama.cpp1", "vLLM2"]);
    expect(currentLocation(router)).toBe("/models/catalog?f=engine");

    await user.click(within(menu).getByRole("menuitemcheckbox", { name: /llama\.cpp/ }));
    expect(currentLocation(router)).toBe("/models/catalog?f=engine&engine=llama-cpp");
    expect(bodyRows()).toEqual(["Alpha 7B"]);
    expect(within(menu).getByRole("menuitemcheckbox", { name: /llama\.cpp/ })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    // A selected engine leaves the other engine's count unaffected, since engines are alternatives.
    expect(within(menu).getByRole("menuitemcheckbox", { name: /vLLM/ })).toHaveTextContent("vLLM2");
    await user.keyboard("{Escape}");

    expect(screen.getByRole("button", { name: "Engine filter: llama.cpp" })).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Remove Engine filter" }));
    expect(currentLocation(router)).toBe("/models/catalog");
    expect(bodyRows()).toEqual(["Alpha 7B", "Bare"]);
    expect(screen.queryByRole("button", { name: /^Engine filter/ })).not.toBeInTheDocument();
  });

  it("keeps a shown predicate without values as an 'any' chip that matches everything", async () => {
    const { router } = renderCatalog("/models/catalog?f=quant");
    await loaded();

    expect(screen.getByRole("button", { name: "Quantization filter: any" })).toBeVisible();
    expect(bodyRows()).toEqual(["Alpha 7B", "Bare"]);
    expect(screen.getByRole("button", { name: /^Reset/ })).toBeVisible();
    expect(currentLocation(router)).toBe("/models/catalog?f=quant");
  });

  it("reads the matching and total profiles of a model whose profiles filters hide", async () => {
    const { user } = renderCatalog("/models/catalog?f=cap&cap=image");
    await loaded();

    expect(bodyRows()).toEqual(["Alpha 7B"]);
    expect(footer()).toHaveTextContent("1 of 2 models");
    expect(cells("Alpha 7B")[0]).toBe("Alpha 7BVision · Chat, Decisions · 2 of 5 profiles");
    expect(cells("Alpha 7B")[2]).toBe("7–8.4 GiB");

    await user.click(screen.getByRole("button", { name: "Show profiles for Alpha 7B" }));
    expect(bodyRows()).toEqual(["Alpha 7B", "Alpha 7B · Quality", "Alpha 7B · Fast"]);
    expect(cells("Alpha 7B · Fast")[0]).toBe("Fastllama.cppMTPNVIDIAQ4_K_M");
    expect(cells("Alpha 7B · Quality")[0]).toBe("Qualityllama.cppMTPNVIDIAQ8_0");
  });

  it("shows the models and profiles offering a speculative method from the Speculative filter", async () => {
    const { user } = renderCatalog("/models/catalog?f=spec&spec=MTP");
    await loaded();

    expect(screen.getByRole("button", { name: "Speculative filter: MTP" })).toBeVisible();
    expect(bodyRows()).toEqual(["Alpha 7B"]);
    expect(footer()).toHaveTextContent("1 of 2 models");

    await user.click(screen.getByRole("button", { name: "Show profiles for Alpha 7B" }));
    expect(bodyRows()).toEqual(["Alpha 7B", "Alpha 7B · Quality", "Alpha 7B · Fast"]);
  });

  it("reads 1 of 5 profiles for a model shown through one profile, and expands to that profile", async () => {
    const { router, user } = renderCatalog("/models/catalog?quant=NVFP4");
    await loaded();

    expect(bodyRows()).toEqual(["Alpha 7B"]);
    expect(cells("Alpha 7B")[0]).toBe("Alpha 7BVision · Chat, Decisions · 1 of 5 profiles");
    await user.click(screen.getByRole("button", { name: "Show profiles for Alpha 7B" }));
    expect(bodyRows()).toEqual(["Alpha 7B", "Alpha 7B · Sized"]);

    await user.click(screen.getByRole("row", { name: "Alpha 7B · Sized" }));
    expect(currentLocation(router)).toBe(
      "/models/catalog?quant=NVFP4&panel=model-catalog%2Fcatalog%3Aalpha-7b&panel=model-catalog%2Fcatalog-profile%3Aalpha-7b%3Asized",
    );
  });

  it("expands a model whose catalog declares one profile", async () => {
    const { user } = renderCatalog();
    await loaded();

    await user.click(screen.getByRole("button", { name: "Show profiles for Bare" }));
    expect(bodyRows()).toEqual(["Alpha 7B", "Bare", "Bare · plain"]);
  });

  it("shows the no-match state and resets the filters from it", async () => {
    const { router, user } = renderCatalog("/models/catalog?engine=vllm&quant=Q4_K_M&sort=-name");
    await loaded();

    expect(await within(table()).findByText("No models match.")).toBeVisible();
    expect(footer()).toHaveTextContent("0 of 2 models");
    await user.click(screen.getByRole("button", { name: "Reset filters" }));

    expect(currentLocation(router)).toBe("/models/catalog?sort=-name");
    expect(bodyRows()).toEqual(["Bare", "Alpha 7B"]);
  });

  it("sorts by Intelligence from its header menu, keeping the unscored model last", async () => {
    const { router, user } = renderCatalog("/models/catalog?sort=-name");
    await loaded();
    expect(bodyRows()).toEqual(["Bare", "Alpha 7B"]);

    await user.click(screen.getByRole("button", { name: "Intelligence column options" }));
    await user.click(await screen.findByRole("menuitem", { name: "High → low" }));
    // High to low is the default sort, so the location writes nothing.
    expect(currentLocation(router)).toBe("/models/catalog");
    expect(bodyRows()).toEqual(["Alpha 7B", "Bare"]);

    await user.click(screen.getByRole("button", { name: "Intelligence column options" }));
    await user.click(await screen.findByRole("menuitem", { name: "Low → high" }));
    expect(currentLocation(router)).toBe("/models/catalog?sort=intelligence");
    expect(bodyRows()).toEqual(["Alpha 7B", "Bare"]);
  });

  it("sorts and hides columns from the header menus", async () => {
    const { router, user } = renderCatalog();
    await loaded();

    await user.click(screen.getByRole("button", { name: "Model column options" }));
    expect(
      within(await screen.findByRole("menu"))
        .getAllByRole("menuitem")
        .map((item) => item.textContent),
    ).toEqual(["A → Z", "Z → A"]);
    await user.click(screen.getByRole("menuitem", { name: "Z → A" }));
    expect(bodyRows()).toEqual(["Bare", "Alpha 7B"]);
    expect(currentLocation(router)).toBe("/models/catalog?sort=-name");

    await user.click(screen.getByRole("button", { name: "Memory column options" }));
    await user.click(await screen.findByRole("menuitem", { name: "Low → high" }));
    expect(currentLocation(router)).toBe("/models/catalog?sort=memory");
    expect(
      screen.getByRole("button", { name: "Memory column options" }).closest("th"),
    ).toHaveAttribute("aria-sort", "ascending");
    // Bare has no memory value, so it sorts last in both directions.
    expect(bodyRows()).toEqual(["Alpha 7B", "Bare"]);
    await user.click(screen.getByRole("button", { name: "Memory column options" }));
    await user.click(await screen.findByRole("menuitem", { name: "High → low" }));
    expect(currentLocation(router)).toBe("/models/catalog?sort=-memory");
    expect(bodyRows()).toEqual(["Alpha 7B", "Bare"]);

    // Hiding the sorted column returns to the default sort.
    await user.click(screen.getByRole("button", { name: "Memory column options" }));
    await user.click(await screen.findByRole("menuitem", { name: "Hide column" }));
    expect(headers()).toEqual(["Model", "Intelligence", "Maximum context", "Total context"]);
    expect(currentLocation(router)).toBe(
      "/models/catalog?cols=model%2Cintelligence%2Cmax-context%2Ctotal-context",
    );
  });

  it("toggles Engine, Quantization, and License from the View menu and resets the columns", async () => {
    const { router, user } = renderCatalog();
    await loaded();

    await user.click(screen.getByRole("button", { name: "View" }));
    const menu = await screen.findByRole("menu");
    expect(
      within(menu)
        .getAllByRole("menuitemcheckbox")
        .map((item) => [item.textContent, item.getAttribute("aria-checked")]),
    ).toEqual([
      ["Intelligence", "true"],
      ["Memory", "true"],
      ["Maximum context", "true"],
      ["Total context", "true"],
      ["Engine", "false"],
      ["Quantization", "false"],
      ["Speculative", "false"],
      ["License", "false"],
    ]);
    await user.click(within(menu).getByRole("menuitemcheckbox", { name: "Engine" }));
    await user.click(within(menu).getByRole("menuitemcheckbox", { name: "Quantization" }));
    await user.click(within(menu).getByRole("menuitemcheckbox", { name: "Speculative" }));
    await user.click(within(menu).getByRole("menuitemcheckbox", { name: "License" }));

    expect(headers()).toEqual([
      "Model",
      "Intelligence",
      "Memory",
      "Maximum context",
      "Total context",
      "Engine",
      "Quantization",
      "Speculative",
      "License",
    ]);
    expect(cells("Alpha 7B").slice(5)).toEqual([
      "vLLMllama.cpp",
      "NVFP4Q4_K_MQ8_0",
      "MTP",
      "Apache-2.0",
    ]);
    expect(cells("Bare").slice(5)).toEqual(["vLLM", "—", "—", ""]);
    expect(currentLocation(router)).toBe(
      "/models/catalog?cols=model%2Cintelligence%2Cmemory%2Cmax-context%2Ctotal-context%2Cengine%2Cquant%2Cspeculative%2Clicense",
    );

    await user.click(within(menu).getByRole("menuitem", { name: "Reset columns" }));
    expect(headers()).toEqual([
      "Model",
      "Intelligence",
      "Memory",
      "Maximum context",
      "Total context",
    ]);
    expect(currentLocation(router)).toBe("/models/catalog");
  });

  it("shows the Speculative column as methods on a model and the default method on each profile", async () => {
    const { user } = renderCatalog("/models/catalog?cols=model,speculative");
    await loaded();
    await user.click(screen.getByRole("button", { name: "Show profiles for Alpha 7B" }));

    expect(cells("Alpha 7B").slice(1)).toEqual(["MTP"]);
    expect(cells("Bare").slice(1)).toEqual(["—"]);
    // Defaults 3 on Fast, 0 on Quality, and no option on the vLLM and CPU configurations.
    expect(cells("Alpha 7B · Fast").slice(1)).toEqual(["MTP"]);
    expect(cells("Alpha 7B · Quality").slice(1)).toEqual(["Off"]);
    expect(cells("Alpha 7B · Sized").slice(1)).toEqual(["—"]);
  });

  it("opens the catalog model panel from a model row and the profile sub-screen from a profile row", async () => {
    const { router, user } = renderCatalog();
    await loaded();

    await user.click(screen.getByRole("button", { name: "Show profiles for Alpha 7B" }));
    expect(currentLocation(router)).toBe("/models/catalog");
    expect(screen.queryByRole("complementary")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Hide profiles for Alpha 7B" })).toHaveAttribute(
      "aria-expanded",
      "true",
    );

    await user.click(screen.getByRole("row", { name: "Alpha 7B" }));
    expect(currentLocation(router)).toBe(
      "/models/catalog?panel=model-catalog%2Fcatalog%3Aalpha-7b",
    );
    expect(await screen.findByRole("complementary", { name: "Alpha 7B" })).toHaveTextContent(
      "Catalog model",
    );

    await user.click(screen.getByRole("row", { name: "Alpha 7B · Quality" }));
    expect(currentLocation(router)).toBe(
      "/models/catalog?panel=model-catalog%2Fcatalog%3Aalpha-7b&panel=model-catalog%2Fcatalog-profile%3Aalpha-7b%3Aquality",
    );
    const panel = await screen.findByRole("complementary", { name: "Alpha 7B · quality" });
    expect(within(panel).getByRole("region", { name: "Profile" })).toBeVisible();

    await user.click(within(panel).getByRole("button", { name: "Back to Alpha 7B" }));
    expect(currentLocation(router)).toBe(
      "/models/catalog?panel=model-catalog%2Fcatalog%3Aalpha-7b",
    );
  });

  it("opens a row panel from the keyboard", async () => {
    const { router, user } = renderCatalog();
    await loaded();

    screen.getByRole("row", { name: "Bare" }).focus();
    await user.keyboard("{Enter}");
    expect(currentLocation(router)).toBe("/models/catalog?panel=model-catalog%2Fcatalog%3Abare");
  });

  it("restores the view from a URL", async () => {
    renderCatalog("/models/catalog?q=a&f=engine&engine=vllm&sort=-name&cols=model,memory,engine");
    await loaded();

    expect(screen.getByRole("searchbox", { name: "Filter models" })).toHaveValue("a");
    expect(screen.getByRole("button", { name: "Engine filter: vLLM" })).toBeVisible();
    expect(headers()).toEqual(["Model", "Memory", "Engine"]);
    expect(
      screen.getByRole("button", { name: "Model column options" }).closest("th"),
    ).toHaveAttribute("aria-sort", "descending");
    expect(bodyRows()).toEqual(["Bare", "Alpha 7B"]);
    expect(cells("Alpha 7B")[1]).toBe("≥5.6–22.4 GiB");
    expect(screen.getByRole("button", { name: /^Reset/ })).toBeVisible();
  });

  it("keeps a filter changed while a panel is open after going back and closing the panel", async () => {
    const { router, user } = renderCatalog();
    await loaded();

    await user.click(screen.getByRole("row", { name: "Alpha 7B" }));
    const panel = await screen.findByRole("complementary", { name: "Alpha 7B" });
    await user.click(within(panel).getByRole("row", { name: "Fast" }));
    await screen.findByRole("complementary", { name: "Alpha 7B · fast" });

    await user.type(screen.getByRole("searchbox", { name: "Filter models" }), "alp");
    expect(currentLocation(router)).toBe(
      "/models/catalog?q=alp&panel=model-catalog%2Fcatalog%3Aalpha-7b&panel=model-catalog%2Fcatalog-profile%3Aalpha-7b%3Afast",
    );

    await user.click(screen.getByRole("button", { name: "Back to Alpha 7B" }));
    await waitFor(() =>
      expect(currentLocation(router)).toBe(
        "/models/catalog?q=alp&panel=model-catalog%2Fcatalog%3Aalpha-7b",
      ),
    );

    await user.click(await screen.findByRole("button", { name: "Close inspector" }));
    await waitFor(() => expect(currentLocation(router)).toBe("/models/catalog?q=alp"));
    expect(screen.queryByRole("complementary")).not.toBeInTheDocument();
    expect(screen.getByRole("searchbox", { name: "Filter models" })).toHaveValue("alp");
    expect(bodyRows()).toEqual(["Alpha 7B"]);
  });

  it("restores each page entry's own view on browser Back and Forward", async () => {
    const { router } = renderCatalog("/models/catalog?q=alpha");
    await loaded();

    // A sidebar link pushes the default view.
    await act(() => router.navigate("/models/catalog"));
    expect(screen.getByRole("searchbox", { name: "Filter models" })).toHaveValue("");
    expect(bodyRows()).toEqual(["Alpha 7B", "Bare"]);

    await act(() => router.navigate(-1));
    await waitFor(() => expect(currentLocation(router)).toBe("/models/catalog?q=alpha"));
    expect(screen.getByRole("searchbox", { name: "Filter models" })).toHaveValue("alpha");
    expect(bodyRows()).toEqual(["Alpha 7B"]);

    await act(() => router.navigate(1));
    await waitFor(() => expect(currentLocation(router)).toBe("/models/catalog"));
    expect(screen.getByRole("searchbox", { name: "Filter models" })).toHaveValue("");
  });
});
