import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { type ComponentProps } from "react";
import { describe, expect, it, vi } from "vitest";

import { type ProfileRow, projectProfileRows } from "./catalog-model-row";
import { ProfileNameCell } from "./catalog-profile-cells";
import { runsOnField } from "./catalog-profile-fields";
import { nvidiaEstimates, syntheticCatalog } from "./catalog-table-test-support";
import { type ProfileRunsOn, ProfileTable } from "./profile-table";

// Sized profiles of the synthetic Alpha model; the others keep their declared minimum or system RAM.
const estimates = nvidiaEstimates([
  ["alpha-7b", "fast", 7_500_000_000n],
  ["alpha-7b", "quality", 9_000_000_000n],
]);

function alphaProfiles(): ProfileRow[] {
  const { engineReleases, hardwareClasses, models } = syntheticCatalog();
  return projectProfileRows(models[0], { engineReleases, estimates, hardwareClasses });
}

function renderTable(props: Partial<ComponentProps<typeof ProfileTable>> = {}) {
  const onSelect = vi.fn<(profileId: string) => void>();
  render(
    <ProfileTable
      estimatesPending={false}
      onSelect={onSelect}
      profiles={alphaProfiles()}
      selectedProfileId="fast"
      {...props}
    />,
  );
  return { onSelect, user: userEvent.setup() };
}

function renderTableWithRerender(props: Partial<ComponentProps<typeof ProfileTable>> = {}) {
  const element = (overrides: Partial<ComponentProps<typeof ProfileTable>>) => (
    <ProfileTable
      estimatesPending={false}
      onSelect={vi.fn()}
      profiles={alphaProfiles()}
      selectedProfileId="fast"
      {...overrides}
    />
  );
  const { rerender } = render(element(props));
  return {
    rerender: (overrides: Partial<ComponentProps<typeof ProfileTable>>) =>
      rerender(element(overrides)),
    user: userEvent.setup(),
  };
}

// A Runs on filter over one machine, where the listed profiles run, starting on that machine.
function runsOn(
  running: string[],
  label = "Workstation",
): { runsOn: ProfileRunsOn; start: { field: "runsOn"; values: string[] }[] } {
  return {
    runsOn: runsOnFilter(running, label),
    start: [{ field: "runsOn", values: [`node:${label}`] }],
  };
}

function runsOnFilter(running: string[], label = "Workstation"): ProfileRunsOn {
  const scope = { kind: "node", siteNodeId: label } as const;
  return {
    field: runsOnField([{ label, scope }], (profile) => running.includes(profile.profileId)),
    noneRun: () => ({
      reasons: ["Needs an NVIDIA GPU", "Workstation isn't reporting"],
      title: `No profile can run on ${label}`,
    }),
  };
}

function footer(): string {
  return screen.getByText(/^\d+ of \d+ profiles/).textContent;
}

function table() {
  return screen.getByRole("table", { name: "Model profiles" });
}

function headers(): string[] {
  return within(table())
    .getAllByRole("columnheader")
    .map((header) => header.textContent);
}

// Profile rows by name, plus the text of any other body row.
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

describe("ProfileTable", () => {
  it("lists the profiles by memory, largest first, in fixed columns", () => {
    renderTable();

    expect(headers()).toEqual([
      "Selected",
      "Profile",
      "Intelligence",
      "Memory",
      "Maximum context",
      "Total context",
    ]);
    expect(bodyRows()).toEqual(["Sized", "cpu", "Quality", "Fast", "Orphan"]);
    expect(cells("Fast")).toEqual([
      "",
      "Fastllama.cppMTPNVIDIAQ4_K_M",
      "41.5",
      "7 GiB",
      "256k",
      "—",
    ]);
    expect(cells("Sized").slice(4)).toEqual(["64k", "—"]);
    expect(screen.getByText("5 of 5 profiles")).toBeVisible();
    expect(screen.queryByRole("button", { name: "View" })).not.toBeInTheDocument();
  });

  it("orders the profiles by a value sort", async () => {
    const { user } = renderTable();

    await user.click(screen.getByRole("button", { name: "Memory column options" }));
    await user.click(await screen.findByRole("menuitem", { name: "High → low" }));

    expect(bodyRows()).toEqual(["Sized", "cpu", "Quality", "Fast", "Orphan"]);
  });

  it("searches profile names and ids", async () => {
    const { user } = renderTable();

    await user.type(screen.getByRole("searchbox", { name: "Filter profiles" }), "QUAL");
    expect(bodyRows()).toEqual(["Quality"]);
    expect(screen.getByText("1 of 5 profiles")).toBeVisible();

    await user.clear(screen.getByRole("searchbox", { name: "Filter profiles" }));
    await user.type(screen.getByRole("searchbox", { name: "Filter profiles" }), "cp");
    expect(bodyRows()).toEqual(["cpu"]);
  });

  it("filters by Engine from counted values", async () => {
    const { user } = renderTable();

    await user.click(screen.getByRole("button", { name: "Filter" }));
    await user.click(await screen.findByRole("menuitem", { name: "Engine" }));
    const menu = await screen.findByRole("menu");
    expect(
      within(menu)
        .getAllByRole("menuitemcheckbox")
        .map((item) => item.textContent),
    ).toEqual(["llama.cpp3", "vLLM2"]);

    await user.click(within(menu).getByRole("menuitemcheckbox", { name: /vLLM/ }));
    await user.keyboard("{Escape}");

    expect(bodyRows()).toEqual(["Sized", "Orphan"]);
    expect(screen.getByText("2 of 5 profiles")).toBeVisible();
    expect(screen.getByRole("button", { name: "Engine filter: vLLM" })).toBeVisible();
  });

  it("filters by Speculative to the profiles whose configuration offers the method", async () => {
    const { user } = renderTable();

    await user.click(screen.getByRole("button", { name: "Filter" }));
    await user.click(await screen.findByRole("menuitem", { name: "Speculative" }));
    const menu = await screen.findByRole("menu");
    expect(
      within(menu)
        .getAllByRole("menuitemcheckbox")
        .map((item) => item.textContent),
    ).toEqual(["MTP2"]);

    await user.click(within(menu).getByRole("menuitemcheckbox", { name: /MTP/ }));
    await user.keyboard("{Escape}");

    // Quality defaults MTP off but still offers it.
    expect(bodyRows()).toEqual(["Quality", "Fast"]);
    expect(screen.getByText("2 of 5 profiles")).toBeVisible();
    expect(screen.getByRole("button", { name: "Speculative filter: MTP" })).toBeVisible();
  });

  it("selects a profile from its row", async () => {
    const { onSelect, user } = renderTable();

    expect(screen.getByRole("radio", { name: "Fast" })).toBeChecked();
    await user.click(screen.getByRole("row", { name: "Quality" }));

    expect(onSelect).toHaveBeenCalledWith("quality");
  });

  it("lists profiles an availability source rules out last, disabled, with its status", () => {
    renderTable({
      availability: (profile) =>
        profile.profileId === "fast" || profile.profileId === "orphan"
          ? { selectable: false, status: { text: "Needs a newer driver", tone: "blocked" } }
          : profile.profileId === "sized"
            ? { selectable: true, status: { text: "Below the memory minimum", tone: "advisory" } }
            : undefined,
    });

    expect(bodyRows()).toEqual(["Sized", "cpu", "Quality", "Fast", "Orphan"]);
    expect(screen.getByRole("radio", { name: "Fast" })).toBeDisabled();
    expect(screen.getByRole("row", { name: "Orphan" })).toHaveAttribute("aria-disabled", "true");
    expect(screen.getByRole("radio", { name: "Sized" })).toBeEnabled();
    expect(
      within(screen.getByRole("row", { name: "Fast" })).getByText("Needs a newer driver"),
    ).toHaveClass("text-destructive");
    expect(
      within(screen.getByRole("row", { name: "Sized" })).getByText("Below the memory minimum"),
    ).toHaveClass("text-warning-foreground");
  });
});

describe("ProfileTable Runs on", () => {
  it("starts with the Runs on chip and shows the profiles it hides once removed", async () => {
    const { user } = renderTable(runsOn(["fast", "quality", "sized"]));

    expect(screen.getByRole("button", { name: "Runs on filter: Workstation" })).toBeVisible();
    expect(bodyRows()).toEqual(["Sized", "Quality", "Fast"]);
    expect(footer()).toBe("3 of 5 profiles");
    expect(screen.queryByRole("button", { name: /can't run here/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^Reset/ })).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Remove Runs on filter" }));
    expect(screen.queryByRole("button", { name: /^Runs on filter/ })).not.toBeInTheDocument();
    expect(bodyRows()).toEqual(["Sized", "cpu", "Quality", "Fast", "Orphan"]);
    expect(footer()).toBe("5 of 5 profiles");

    await user.click(screen.getByRole("button", { name: "Reset" }));
    expect(screen.getByRole("button", { name: "Runs on filter: Workstation" })).toBeVisible();
    expect(bodyRows()).toEqual(["Sized", "Quality", "Fast"]);
  });

  it("keeps the chip when other filters show nothing", async () => {
    const { user } = renderTable(runsOn(["fast", "quality", "sized"]));

    await user.type(screen.getByRole("searchbox", { name: "Filter profiles" }), "cp");
    expect(bodyRows()).toEqual(["No profiles match. Reset filters"]);
    expect(footer()).toBe("0 of 5 profiles");

    await user.click(screen.getByRole("button", { name: "Reset filters" }));
    expect(screen.getByRole("searchbox", { name: "Filter profiles" })).toHaveValue("");
    expect(screen.getByRole("button", { name: "Runs on filter: Workstation" })).toBeVisible();
    expect(bodyRows()).toEqual(["Sized", "Quality", "Fast"]);
  });

  it("says why no profile runs and offers every profile", async () => {
    const { user } = renderTable(runsOn([]));

    expect(bodyRows()).toEqual([
      "No profile can run on WorkstationNeeds an NVIDIA GPUWorkstation isn't reportingShow all profiles",
    ]);
    expect(footer()).toBe("0 of 5 profiles");

    await user.click(screen.getByRole("button", { name: "Show all profiles" }));
    expect(bodyRows()).toEqual(["Sized", "cpu", "Quality", "Fast", "Orphan"]);
  });

  it("puts the chip back on a new starting value, keeping the search", async () => {
    const { rerender, user } = renderTableWithRerender(runsOn(["fast", "quality", "sized"]));

    await user.click(screen.getByRole("button", { name: "Remove Runs on filter" }));
    await user.type(screen.getByRole("searchbox", { name: "Filter profiles" }), "t");
    expect(bodyRows()).toEqual(["Quality", "Fast"]);

    rerender(runsOn(["quality"], "Server"));
    expect(screen.getByRole("button", { name: "Runs on filter: Server" })).toBeVisible();
    expect(screen.getByRole("searchbox", { name: "Filter profiles" })).toHaveValue("t");
    expect(bodyRows()).toEqual(["Quality"]);

    rerender({ runsOn: undefined });
    expect(screen.queryByRole("button", { name: /^Runs on filter/ })).not.toBeInTheDocument();
    expect(bodyRows()).toEqual(["Quality", "Fast"]);
  });
});

describe("ProfileTable starting chips", () => {
  const start = [
    { field: "runsOn" as const, values: ["node:Workstation"] },
    { field: "engine" as const, values: ["llama-cpp"] },
  ];

  it("applies the profile chips at once, adds Runs on with its field, and resets to them", async () => {
    const { rerender, user } = renderTableWithRerender({ start });
    expect(screen.getByRole("button", { name: "Engine filter: llama.cpp" })).toBeVisible();
    expect(screen.queryByRole("button", { name: /^Runs on filter/ })).not.toBeInTheDocument();
    expect(bodyRows()).toEqual(["cpu", "Quality", "Fast"]);
    expect(screen.queryByRole("button", { name: /^Reset/ })).not.toBeInTheDocument();

    await user.type(screen.getByRole("searchbox", { name: "Filter profiles" }), "s");
    rerender({ runsOn: runsOnFilter(["fast", "sized"]), start });
    expect(screen.getByRole("button", { name: "Runs on filter: Workstation" })).toBeVisible();
    expect(screen.getByRole("button", { name: "Engine filter: llama.cpp" })).toBeVisible();
    expect(screen.getByRole("searchbox", { name: "Filter profiles" })).toHaveValue("s");
    expect(bodyRows()).toEqual(["Fast"]);

    await user.click(screen.getByRole("button", { name: "Remove Engine filter" }));
    expect(bodyRows()).toEqual(["Sized", "Fast"]);
    await user.click(screen.getByRole("button", { name: "Reset" }));
    expect(screen.getByRole("button", { name: "Engine filter: llama.cpp" })).toBeVisible();
    expect(screen.getByRole("button", { name: "Runs on filter: Workstation" })).toBeVisible();
    expect(screen.getByRole("searchbox", { name: "Filter profiles" })).toHaveValue("");
    expect(bodyRows()).toEqual(["Fast"]);
  });
});

describe("ProfileNameCell pills", () => {
  it("shows the engine pill", () => {
    renderName({ engineKinds: ["vllm"] });

    expect(screen.getByTitle("Inference engine")).toHaveTextContent("vLLM");
  });

  it("shows a pill per offered speculative method, whatever its default", () => {
    renderTable();

    for (const name of ["Fast", "Quality"]) {
      expect(
        within(screen.getByRole("row", { name }))
          .getAllByTitle("Speculative decoding")
          .map((pill) => pill.textContent),
      ).toEqual(["MTP"]);
    }
    for (const name of ["Sized", "cpu", "Orphan"]) {
      expect(
        within(screen.getByRole("row", { name })).queryByTitle("Speculative decoding"),
      ).not.toBeInTheDocument();
    }
  });

  it("shows one hardware pill per vendor, in configuration order", () => {
    renderName({
      acceleratorRequirements: [
        requirement("nvidia-unified", "NVIDIA · unified memory", "nvidia"),
        requirement("nvidia-dedicated", "NVIDIA · dedicated memory", "nvidia"),
        requirement("apple", "Apple Silicon", "apple"),
      ],
    });

    expect(screen.getAllByTitle("Supported hardware").map((pill) => pill.textContent)).toEqual([
      "NVIDIA",
      "Apple Silicon",
    ]);
  });

  it("names a single-class vendor by its vendor, not its class", () => {
    renderName({
      acceleratorRequirements: [
        requirement("nvidia-dedicated", "NVIDIA · dedicated memory", "nvidia"),
      ],
    });

    expect(screen.getAllByTitle("Supported hardware").map((pill) => pill.textContent)).toEqual([
      "NVIDIA",
    ]);
  });

  it("shows no hardware pill without a supported accelerator class", () => {
    renderName({ acceleratorRequirements: [] });

    expect(screen.queryByTitle("Supported hardware")).not.toBeInTheDocument();
  });
});

function renderName(values: Partial<ProfileRow>) {
  render(<ProfileNameCell profile={{ ...alphaProfiles()[0], ...values }} />);
}

function requirement(hardwareClassId: string, displayName: string, vendor: string) {
  return { displayName, hardwareClassId, minimumMemoryBytesPerDevice: 0, vendors: [vendor] };
}
