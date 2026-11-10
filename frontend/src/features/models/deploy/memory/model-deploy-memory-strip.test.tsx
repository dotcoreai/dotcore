import { create } from "@bufbuild/protobuf";
import { cleanup, render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { runningDeployment } from "@/features/memory/node-memory-test-support";
import { type ResourceEstimate } from "@/features/models/deploy/use-resource-estimate";
import { ApplicationDeploymentSchema } from "@/gen/dotcore/application/v1/deployment_pb";
import { MemoryFit } from "@/gen/dotcore/deployment/v1/memory_fit_pb";
import { ExactPlacementSchema } from "@/gen/dotcore/deployment/v1/placement_pb";
import { setupUser } from "@/test/user";
import { getResourceColor } from "@/theme/resource-color";
import { ResourceColorContext } from "@/theme/resource-color-context";

import { previewMemory, type PreviewMemoryInput, type PreviewTarget } from "./model-deploy-memory";
import { MemoryCapacityWarning, ModelDeployMemoryStrip } from "./model-deploy-memory-strip";
import {
  application,
  claimed,
  dedicatedPreview,
  devicePool,
  estimated,
  exceeding,
  gib,
  hostPool,
  measured,
  mib,
  other,
  placementPreview,
  previewed,
} from "./model-deploy-memory-test-support";

const nextColor = "#123456";

const target: PreviewTarget = {
  executionTargetId: "gpu1-5090",
  name: "gpu1 / NVIDIA GeForce RTX 5090",
  nodeName: "gpu1",
  siteNodeId: "gpu1",
};

describe("ModelDeployMemoryStrip", () => {
  it("draws a dedicated target's pools from the preview, this deployment on top", async () => {
    const user = setupUser();
    renderStrip({
      estimate: settled(
        placementPreview([
          devicePool(32n * gib, [
            measured("d-gemma", 10n * gib),
            other(2n * gib),
            previewed(8n * gib),
          ]),
          hostPool(64n * gib, [other(34n * gib), previewed(6n * gib)]),
        ]),
      ),
    });

    const strip = screen.getByRole("region", { name: "Predicted memory" });
    expect(within(strip).getByText("gpu1 / NVIDIA GeForce RTX 5090")).toBeVisible();
    const [vram, ram] = within(strip).getAllByRole("img");
    expect(vram).toHaveAttribute(
      "aria-label",
      "VRAM on gpu1: 20 GiB used of 32 GiB; deployments: Gemma 10 GiB, Test Model 8 GiB",
    );
    expect(ram).toHaveAttribute(
      "aria-label",
      "RAM on gpu1: 40 GiB used of 64 GiB; deployments: Test Model 6 GiB",
    );
    // This deployment takes the site's next free color; the running model keeps its own, grayed.
    const [gemma, prediction] = [...vram.children] as HTMLElement[];
    expect(gemma.className).toContain("grayscale");
    expect(gemma.style.backgroundColor).toBe(cssColor(getResourceColor("d-gemma")));
    expect(prediction.className).not.toContain("grayscale");
    expect(prediction.style.backgroundColor).toBe(cssColor(nextColor));

    const vramReadout = within(strip).getByRole("button", { name: "VRAM 8 / 32 GiB" });
    expect(vramReadout).toHaveAccessibleDescription(
      /^This deployment: 8 GiB \(predicted\)\. VRAM: dedicated GPU memory\. 20 GiB used after deploy · 12 GiB free\. Observed /,
    );
    expect(within(strip).getByRole("button", { name: "RAM 6 / 64 GiB" })).toBeVisible();
    expect(strip.querySelector('[data-slot="memory-capacity-marker"]')).toBeNull();
    expect(screen.queryByRole("note")).not.toBeInTheDocument();

    await user.hover(prediction);

    expect(await screen.findByText("gpu1 · 8 GiB (predicted)")).toBeVisible();
  });

  it("draws a unified target as one RAM row holding this deployment", () => {
    const spark = {
      executionTargetId: "spark-gb10",
      name: "spark / NVIDIA GB10",
      nodeName: "spark",
      siteNodeId: "spark",
    };
    renderStrip({
      estimate: settled(
        placementPreview([
          hostPool(128n * gib, [
            measured("d-qwen", 46n * gib),
            other(22n * gib),
            previewed(40n * gib),
          ]),
        ]),
        spark,
      ),
      target: spark,
    });

    const strip = screen.getByRole("region", { name: "Predicted memory" });
    expect(
      within(strip)
        .getAllByRole("img")
        .map((bar) => bar.getAttribute("aria-label")),
    ).toEqual([
      "RAM on spark: 108 GiB used of 128 GiB; deployments: Qwen 46 GiB, Test Model 40 GiB",
    ]);
    expect(within(strip).getByRole("button", { name: "RAM 40 / 128 GiB" })).toBeVisible();
    expect(within(strip).queryByRole("button", { name: /^VRAM/ })).not.toBeInTheDocument();
  });

  it("marks a pool beyond its total with a marker, a ▲ readout, and a Deploy warning", () => {
    const memory = renderStrip({
      estimate: settled(
        dedicatedPreview({ accelerator: 26n * gib, device: exceeding(6n * gib, true) }),
      ),
    });

    expect(memory.kind === "ready" && memory.overCapacity).toEqual([
      { beyondTotal: true, bytes: 6n * gib, headroomPercent: 1, label: "VRAM" },
    ]);
    const strip = screen.getByRole("region", { name: "Predicted memory" });
    const readout = within(strip).getByRole("button", {
      name: "VRAM 26 / 32 GiB, over capacity by 6 GiB",
    });
    expect(readout).toHaveTextContent(/^▲ 26 \/ 32 GiB$/);
    expect(readout.className).toContain("text-destructive");
    expect(capacityMarkers(strip)).toEqual([["total", `${(32 / 38) * 100}%`]]);
    expect(
      within(strip).getByRole("button", { name: "RAM — / 64 GiB" }),
    ).toHaveAccessibleDescription(/^This deployment: no extra host memory\. /);
    expect(screen.getByRole("note")).toHaveTextContent(
      "This deployment may exceed memory capacity on gpu1 / NVIDIA GeForce RTX 5090: VRAM by 6 GiB.",
    );
  });

  it("warns within the headroom from the pool's verdict while the bar stays under the total", () => {
    renderStrip({
      estimate: settled(
        dedicatedPreview({ accelerator: 20_300n * mib, device: exceeding(147n * mib, false) }),
      ),
    });

    // The bar marks the pool's fit line, apart from the total's marker and ▲.
    const strip = screen.getByRole("region", { name: "Predicted memory" });
    const readout = within(strip).getByRole("button", {
      name: "VRAM 20 / 32 GiB, less than 1% free by 147 MiB",
    });
    expect(readout).toHaveTextContent(/^20 \/ 32 GiB$/);
    expect(readout.className).toContain("text-warning-foreground");
    expect(capacityMarkers(strip)).toEqual([
      ["fit", `${(Number(32n * gib - (32n * gib) / 100n) / Number(32n * gib)) * 100}%`],
    ]);
    expect(screen.getByRole("note")).toHaveTextContent(
      "This deployment leaves less than 1% of memory free on gpu1 / NVIDIA GeForce RTX 5090: VRAM by 147 MiB.",
    );
  });

  it("names each pool's warning when one is beyond its total and another within the headroom", () => {
    renderStrip({
      estimate: settled(
        dedicatedPreview({
          accelerator: 26n * gib,
          device: exceeding(6n * gib, true),
          host: 30n * gib - 400n * mib,
          hostVerdict: exceeding(255n * mib, false),
        }),
      ),
    });

    expect(screen.getByRole("note")).toHaveTextContent(
      "This deployment may exceed memory capacity on gpu1 / NVIDIA GeForce RTX 5090: VRAM by 6 GiB. It leaves less than 1% of memory free there: RAM by 255 MiB.",
    );
  });

  it("warns on nothing the backend judges to fit, or that exceeds only on a declared minimum", () => {
    const fits = previewMemory(
      input({ estimate: settled(dedicatedPreview({ accelerator: 8n * gib })) }),
    );
    const minimumOnly = previewMemory(
      input({
        estimate: settled(
          dedicatedPreview({ accelerator: 8n * gib, device: exceeding(0n, false) }),
        ),
      }),
    );

    expect(fits.kind === "ready" && fits.overCapacity).toEqual([]);
    expect(minimumOnly.kind === "ready" && minimumOnly.overCapacity).toEqual([]);
  });

  it("names the GPU of each pool on a target with several", () => {
    const memory = renderStrip({
      estimate: settled(
        placementPreview([
          devicePool(32n * gib, [other(12n * gib), previewed(8n * gib)], { deviceIndex: 0 }),
          devicePool(24n * gib, [other(4n * gib), previewed(22n * gib)], {
            deviceIndex: 1,
            ...exceeding(2n * gib, true),
          }),
          hostPool(64n * gib, [other(34n * gib)]),
        ]),
      ),
    });

    const strip = screen.getByRole("region", { name: "Predicted memory" });
    expect(
      within(strip).getByRole("button", { name: /^VRAM 30 \/ 56 GiB/ }),
    ).toHaveAccessibleDescription(
      /gpu1: 46 GiB \/ 56 GiB\. NVIDIA GeForce RTX 5090 · GPU 0: 20 GiB \/ 32 GiB\. NVIDIA GeForce RTX 5090 · GPU 1: 26 GiB \/ 24 GiB\./,
    );
    expect(memory.kind === "ready" && memory.overCapacity).toEqual([
      {
        beyondTotal: true,
        bytes: 2n * gib,
        headroomPercent: 1,
        label: "VRAM on NVIDIA GeForce RTX 5090 · GPU 1",
      },
    ]);
    // One GPU beyond its total marks the row's total and flags its readout.
    const readout = within(strip).getByRole("button", {
      name: "VRAM 30 / 56 GiB, over capacity by 2 GiB",
    });
    expect(readout).toHaveTextContent(/^▲ 30 \/ 56 GiB$/);
    expect(capacityMarkers(strip)).toEqual([["total", "100%"]]);
  });

  it("draws a starting deployment's measured and claimed use as one starting segment", () => {
    renderStrip({
      deployments: [runningDeployment("d-new", "Llama")],
      estimate: settled(
        placementPreview([
          devicePool(32n * gib, [
            measured("d-new", 1n * gib),
            claimed("d-new", 3n * gib),
            other(2n * gib),
            previewed(8n * gib),
          ]),
          hostPool(64n * gib, [estimated("d-new", 2n * gib), other(30n * gib)]),
        ]),
      ),
    });

    const strip = screen.getByRole("region", { name: "Predicted memory" });
    expect(
      within(strip)
        .getAllByRole("img")
        .map((bar) => bar.getAttribute("aria-label")),
    ).toEqual([
      "VRAM on gpu1: 14 GiB used of 32 GiB; deployments: Llama 4 GiB, Test Model 8 GiB",
      "RAM on gpu1: 32 GiB used of 64 GiB; deployments: Llama 2 GiB; estimated: Llama",
    ]);
  });

  it("draws application segments named after the site's applications, a new one generically", () => {
    renderStrip({
      applications: [
        create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "a-comfy",
          applicationDeploymentName: "ComfyUI",
        }),
      ],
      estimate: settled(
        placementPreview([
          devicePool(32n * gib, [
            measured("d-gemma", 10n * gib),
            application("a-comfy", 3n * gib),
            application("a-new", gib),
            other(2n * gib),
            previewed(8n * gib),
          ]),
        ]),
      ),
    });

    const [vram] = within(screen.getByRole("region", { name: "Predicted memory" })).getAllByRole(
      "img",
    );
    expect(vram).toHaveAttribute(
      "aria-label",
      "VRAM on gpu1: 24 GiB used of 32 GiB; deployments: Gemma 10 GiB, ComfyUI 3 GiB, Application deployment 1 GiB, Test Model 8 GiB",
    );
    expect(
      [...vram.querySelectorAll("[data-deployment-id]")].map((segment) =>
        segment.getAttribute("data-deployment-id"),
      ),
    ).toEqual(["d-gemma", "a-comfy", "a-new", "model-deploy-dialog-prediction"]);
  });

  it("lists a pool of unknown size as unreadable with what it holds, and draws the others", () => {
    renderStrip({
      estimate: settled(
        placementPreview(
          [
            devicePool(undefined, [measured("d-gemma", 10n * gib), previewed(8n * gib)]),
            hostPool(64n * gib, [other(34n * gib), previewed(6n * gib)]),
          ],
          MemoryFit.UNKNOWN,
        ),
      ),
    });

    const strip = screen.getByRole("region", { name: "Predicted memory" });
    expect(
      within(strip)
        .getAllByRole("img")
        .map((bar) => bar.getAttribute("aria-label")),
    ).toEqual([
      "RAM on gpu1: 40 GiB used of 64 GiB; deployments: Test Model 6 GiB; unreadable: gpu1 VRAM (Gemma, Test Model)",
    ]);
    expect(within(strip).queryByRole("button", { name: /^VRAM/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("note")).not.toBeInTheDocument();
  });

  it("shows No estimate where the preview holds no demand", () => {
    renderStrip({ estimate: settled(dedicatedPreview({ host: 6n * gib })) });

    const strip = screen.getByRole("region", { name: "Predicted memory" });
    expect(
      within(strip).getByRole("button", { name: "VRAM No estimate" }),
    ).toHaveAccessibleDescription(
      /^This deployment: no accelerator memory estimate for these settings\. /,
    );
    expect(within(strip).getByRole("button", { name: "RAM 6 / 64 GiB" })).toBeVisible();
  });

  it("shows Estimating… on every row over the held preview once loading feedback is due", () => {
    const held = dedicatedPreview({ accelerator: 8n * gib });
    const loading = renderStrip({
      estimate: { ...settled(held), loadingVisible: true, pending: true },
    });

    const strip = screen.getByRole("region", { name: "Predicted memory" });
    expect(within(strip).getByRole("button", { name: "VRAM Estimating…" })).toHaveTextContent(
      /^Estimating…$/,
    );
    expect(within(strip).getByRole("button", { name: "RAM Estimating…" })).toBeVisible();
    expect(loading.kind === "ready" && loading.focusStatus).toEqual({
      ram: { detail: "This deployment: estimating memory", readout: "Estimating…" },
      vram: { detail: "This deployment: estimating memory", readout: "Estimating…" },
    });

    const quick = previewMemory(input({ estimate: { ...settled(held), pending: true } }));
    expect(quick.kind === "ready" && quick.focusStatus).toEqual({});
  });

  it("dims a held preview whose refresh failed", () => {
    renderStrip({
      estimate: { ...settled(dedicatedPreview({ accelerator: 8n * gib })), stale: true },
    });

    expect(
      [...document.querySelectorAll("[data-stale]")].map((reading) =>
        reading.getAttribute("data-stale"),
      ),
    ).toEqual(["true", "true"]);
  });

  it("draws a held preview only for its own target, with no Deploy warning for another", () => {
    const held = dedicatedPreview({ accelerator: 26n * gib, device: exceeding(6n * gib, true) });
    const refreshing = renderStrip({
      estimate: { ...settled(held), pending: true },
    });

    // A refresh for the same target keeps drawing, and warning about, what it last read.
    expect(refreshing.kind).toBe("ready");
    expect(screen.getByRole("note")).toHaveTextContent(/^This deployment may exceed memory/);
    cleanup();

    const switched = renderStrip({
      estimate: { ...settled(held), pending: true },
      target: { ...target, executionTargetId: "gpu1-other", name: "gpu1 / Other GPU" },
    });

    // The previous target's preview is neither drawn nor read for a warning on the new one.
    expect(switched).toEqual({ kind: "loading", targetName: "gpu1 / Other GPU" });
    const strip = screen.getByRole("region", { name: "Predicted memory" });
    expect(strip).toHaveTextContent(/^gpu1 \/ Other GPURAM—$/);
    expect(screen.queryByRole("note")).not.toBeInTheDocument();
  });

  it("says why nothing draws: loading, a failed estimate, no preview, or no target", () => {
    const { rerender } = render(
      <ModelDeployMemoryStrip
        memory={previewMemory(input({ estimate: { loadingVisible: false, pending: true } }))}
      />,
    );

    const strip = screen.getByRole("region", { name: "Predicted memory" });
    expect(strip).toHaveTextContent(/^gpu1 \/ NVIDIA GeForce RTX 5090RAM—$/);

    rerender(
      <ModelDeployMemoryStrip
        memory={previewMemory(
          input({
            estimate: {
              error: "Resource estimate unavailable: sizing failed.",
              loadingVisible: false,
              pending: false,
            },
          }),
        )}
      />,
    );
    expect(strip).toHaveTextContent(/^gpu1 \/ NVIDIA GeForce RTX 5090Estimate unavailable\.$/);

    rerender(
      <ModelDeployMemoryStrip
        memory={previewMemory(input({ estimate: { loadingVisible: false, pending: false } }))}
      />,
    );
    expect(strip).toHaveTextContent(
      /^gpu1 \/ NVIDIA GeForce RTX 5090Memory readings unavailable\.$/,
    );

    rerender(<ModelDeployMemoryStrip memory={previewMemory(input({ target: undefined }))} />);
    expect(strip).toHaveTextContent(/^No execution target$/);

    rerender(
      <ModelDeployMemoryStrip
        memory={previewMemory(input({ target: undefined, targetsLoading: true }))}
      />,
    );
    expect(strip).toHaveTextContent(/^RAM—$/);
    expect(within(strip).queryAllByRole("img")).toEqual([]);
  });
});

function renderStrip(overrides: Partial<PreviewMemoryInput>) {
  const memory = previewMemory(input(overrides));
  render(
    <ResourceColorContext.Provider value={{ colorOf: getResourceColor, nextColor }}>
      <ModelDeployMemoryStrip memory={memory} />
      <MemoryCapacityWarning memory={memory} />
    </ResourceColorContext.Provider>,
  );
  return memory;
}

function input(overrides: Partial<PreviewMemoryInput>): PreviewMemoryInput {
  return {
    deployment: { name: "Test Model", profileId: "default" },
    deployments: [runningDeployment("d-gemma", "Gemma"), runningDeployment("d-qwen", "Qwen")],
    estimate: settled(dedicatedPreview({ accelerator: 8n * gib })),
    target,
    targetsLoading: false,
    ...overrides,
  };
}

/** A settled estimate read for `readFor`, the default target unless given. */
function settled(
  placementPreview: ResourceEstimate["placementPreview"],
  readFor: PreviewTarget = target,
): ResourceEstimate {
  return {
    loadingVisible: false,
    pending: false,
    placement: create(ExactPlacementSchema, {
      executionTargetId: readFor.executionTargetId,
      siteNodeId: readFor.siteNodeId,
    }),
    placementPreview,
  };
}

/** The bar markers in `container`: the line each marks and where it sits. */
function capacityMarkers(container: HTMLElement): [string | undefined, string][] {
  return [...container.querySelectorAll<HTMLElement>('[data-slot="memory-capacity-marker"]')].map(
    (marker) => [marker.dataset.line, marker.style.left],
  );
}

/** The computed form a browser gives an inline hex color. */
function cssColor(hex: string): string {
  const element = document.createElement("span");
  element.style.backgroundColor = hex;
  return element.style.backgroundColor;
}
