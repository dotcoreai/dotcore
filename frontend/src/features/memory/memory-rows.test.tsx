import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it } from "vitest";

import { MetricRows } from "@/components/metrics/metric-row/metric-row";
import { getResourceColor } from "@/lib/resource-color";
import { expectResourceMarker } from "@/test/resource-marker";

import { MemoryHoverProvider } from "./memory-hover";
import { MemoryMetricRows, type MemoryRowCapacity, MemoryRows } from "./memory-rows";
import type { MemoryRow, MemorySegment, MemorySpan } from "./site-memory";

const gib = 1024n ** 3n;

describe("MemoryRows", () => {
  it("draws segments in card colors, then other memory, over a free track", () => {
    render(<MemoryRows rows={[row("vram", [gpu1])]} />);

    const bar = screen.getByRole("img");
    expect(
      [...bar.children].map((segment) => [
        (segment as HTMLElement).style.width,
        (segment as HTMLElement).style.backgroundColor !== "",
      ]),
    ).toEqual([
      [`${(10 / 32) * 100}%`, true],
      [`${(17 / 32) * 100}%`, true],
      [`${(1 / 32) * 100}%`, false],
    ]);
    expect((bar.children[0] as HTMLElement).style.backgroundColor).toBe(
      cssColor(getResourceColor("gemma")),
    );
    expect(bar).toHaveAttribute(
      "aria-label",
      "VRAM on gpu1: 28 GiB used of 32 GiB; deployments: Gemma 4 12B QAT 10 GiB, Qwen3.8 27B 17 GiB; estimated: Qwen3.8 27B",
    );
    expect(screen.getByRole("button", { name: "VRAM 28 / 32 GiB" })).toBeVisible();
  });

  it("merges machines into one bar, or keeps each machine's span named under it with its marker", () => {
    const rows = [row("ram", [sparkOne, sparkTwo])];
    const { rerender } = render(<MemoryRows rows={rows} />);

    expect(screen.getAllByRole("img")).toHaveLength(1);
    expect(screen.queryByText("spark-01")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "RAM 80 / 240 GiB" })).toBeVisible();

    rerender(<MemoryRows mode="spans" rows={rows} />);

    expect(screen.getAllByRole("img").map((bar) => bar.getAttribute("aria-label"))).toEqual([
      "RAM on spark-01: 50 GiB used of 120 GiB; deployments: Qwen3.8 27B 40 GiB",
      "RAM on spark-02: 30 GiB used of 120 GiB",
    ]);
    expect(screen.getByText("spark-01")).toBeVisible();
    expect(screen.getByText("spark-02")).toBeVisible();
    expectResourceMarker(screen.getByText("spark-01"), "spark-01");
    expectResourceMarker(screen.getByText("spark-02"), "spark-02");
  });

  it("keeps the bar within its track when drawn use exceeds the total", () => {
    const overEstimated = span("gpu1", 32n, [segment("qwen", "Qwen3.8 27B", 40n, true)], 0n);
    render(<MemoryRows rows={[row("vram", [overEstimated])]} />);

    const bar = screen.getByRole("img");
    expect((bar.children[0] as HTMLElement).style.width).toBe("100%");
    expect(screen.getByRole("button", { name: "VRAM 40 / 32 GiB" })).toBeVisible();
  });

  it("leaves bars past the total unmarked without a capacity verdict", async () => {
    const user = userEvent.setup();
    const overEstimated = span("gpu1", 32n, [segment("qwen", "Qwen3.8 27B", 40n, true)], 0n);
    const { container } = render(<MemoryRows rows={[row("vram", [overEstimated])]} />);

    const bar = screen.getByRole("img");
    expect(container.querySelector('[data-slot="memory-capacity-marker"]')).toBeNull();
    expect(bar).toHaveAttribute(
      "aria-label",
      "VRAM on gpu1: 40 GiB used of 32 GiB; deployments: Qwen3.8 27B 40 GiB; estimated: Qwen3.8 27B",
    );
    const readout = screen.getByRole("button", { name: "VRAM 40 / 32 GiB" });
    expect(readout).toHaveTextContent(/^40 \/ 32 GiB$/);
    expect(readout.className).toContain("text-muted-foreground");
    expect(readout.className).not.toContain("text-destructive");

    await user.hover(readout);

    expect(await screen.findByText("40 GiB used · 0 bytes free")).toBeVisible();
  });

  it("marks the total and flags the readout when the verdict puts use beyond the total", async () => {
    const user = userEvent.setup();
    const predicted = span(
      "gpu1",
      32n,
      [segment("gemma", "Gemma 4 12B QAT", 10n), segment("next", "Qwen3.8 27B", 26n, true)],
      1n,
    );
    const { container } = render(
      <MemoryRows
        capacity={{ vram: beyondTotal(5n * gib, 32n * gib) }}
        focusDeploymentId="next"
        rows={[row("vram", [predicted])]}
      />,
    );

    const markers = container.querySelectorAll<HTMLElement>('[data-slot="memory-capacity-marker"]');
    expect([...markers].map((marker) => [marker.dataset.line, marker.style.left])).toEqual([
      ["total", `${(32 / 37) * 100}%`],
    ]);
    expect(screen.getByRole("img")).toHaveAttribute(
      "aria-label",
      "VRAM on gpu1: 37 GiB used of 32 GiB; deployments: Gemma 4 12B QAT 10 GiB, Qwen3.8 27B 26 GiB; estimated: Qwen3.8 27B; over capacity by 5 GiB",
    );
    const readout = screen.getByRole("button", {
      name: "VRAM 26 / 32 GiB, over capacity by 5 GiB",
    });
    expect(readout).toHaveTextContent(/^▲ 26 \/ 32 GiB$/);
    expect(readout.className).toContain("text-destructive");

    await user.hover(readout);

    expect(await screen.findByText("37 GiB used · 5 GiB over capacity")).toBeVisible();
  });

  it("marks the fit line in the warning tone when the verdict puts use within the headroom", async () => {
    const user = userEvent.setup();
    const tight = span(
      "gpu1",
      32n,
      [segment("gemma", "Gemma 4 12B QAT", 10n), predictedSegment("next", "Qwen3.8 27B", 20n)],
      2n,
    );
    const { container } = render(
      <MemoryRows
        capacity={{
          vram: { beyondTotal: false, headroomPercent: 1, lineBytes: 31n * gib, overBytes: gib },
        }}
        focusDeploymentId="next"
        rows={[row("vram", [tight])]}
      />,
    );

    const markers = container.querySelectorAll<HTMLElement>('[data-slot="memory-capacity-marker"]');
    expect(
      [...markers].map((marker) => [
        marker.dataset.line,
        marker.style.left,
        marker.className.includes("bg-warning-foreground"),
      ]),
    ).toEqual([["fit", `${(31 / 32) * 100}%`, true]]);
    expect(screen.getByRole("img")).toHaveAttribute(
      "aria-label",
      "VRAM on gpu1: 32 GiB used of 32 GiB; deployments: Gemma 4 12B QAT 10 GiB, Qwen3.8 27B 20 GiB; less than 1% free by 1 GiB",
    );
    const readout = screen.getByRole("button", {
      name: "VRAM 20 / 32 GiB, less than 1% free by 1 GiB",
    });
    expect(readout).toHaveTextContent(/^20 \/ 32 GiB$/);
    expect(readout.className).toContain("text-warning-foreground");
    expect(readout.className).not.toContain("text-destructive");

    await user.hover(readout);

    expect(await screen.findByText("32 GiB used after deploy · less than 1% free")).toBeVisible();
  });

  it("marks nothing on a row without a verdict past a line", () => {
    const { container } = render(
      <MemoryRows capacity={{ ram: beyondTotal(gib, 32n * gib) }} rows={[row("vram", [gpu1])]} />,
    );

    expect(container.querySelector('[data-slot="memory-capacity-marker"]')).toBeNull();
    const readout = screen.getByRole("button", { name: "VRAM 28 / 32 GiB" });
    expect(readout).toHaveTextContent(/^28 \/ 32 GiB$/);
    expect(readout.className).toContain("text-muted-foreground");
  });

  it("reads out a predicted deployment as this deployment, after deploy", async () => {
    const user = userEvent.setup();
    const predicted = span(
      "gpu1",
      32n,
      [segment("gemma", "Gemma 4 12B QAT", 10n), predictedSegment("next", "Qwen3.8 27B", 8n)],
      2n,
    );
    render(<MemoryRows focusDeploymentId="next" rows={[row("vram", [predicted])]} />);

    const readout = screen.getByRole("button", { name: "VRAM 8 / 32 GiB" });
    expect(readout).toHaveAccessibleDescription(
      /^This deployment: 8 GiB \(predicted\)\. VRAM: dedicated GPU memory\. 20 GiB used after deploy · 12 GiB free\. Observed \d+ seconds? ago\.$/,
    );

    await user.hover(screen.getByRole("img").children[1]);

    expect(await screen.findByText("gpu1 · 8 GiB (predicted)")).toBeVisible();
  });

  it("replaces the focused readout with a status and describes it", async () => {
    const user = userEvent.setup();
    render(
      <MemoryRows
        focusDeploymentId="next"
        focusStatus={{
          ram: { detail: "This deployment: no extra host memory" },
          vram: { detail: "This deployment: estimating memory", readout: "Estimating…" },
        }}
        rows={[row("vram", [gpu1]), row("ram", [gpu1Host])]}
      />,
    );

    const vram = screen.getByRole("button", { name: "VRAM Estimating…" });
    expect(vram).toHaveTextContent(/^Estimating…$/);
    expect(vram.className).toContain("text-muted-foreground");
    expect(vram).toHaveAccessibleDescription(
      /^This deployment: estimating memory\. VRAM: dedicated GPU memory\. 28 GiB used · 4 GiB free\./,
    );
    const ram = screen.getByRole("button", { name: "RAM — / 62 GiB" });
    expect(ram).toHaveAccessibleDescription(
      /^This deployment: no extra host memory\. RAM: system memory\. 9 GiB used · 53 GiB free\./,
    );

    await user.hover(vram);

    expect(await screen.findByText("This deployment: estimating memory")).toBeVisible();
  });

  it("names an unreadable pool without a row, and leaves zero-byte segments out of the bar", () => {
    const idle = {
      ...gpu1Host,
      segments: [{ ...segment("idle", "Idle", 0n), machineId: "gpu1" }],
    };
    render(
      <MemoryRows
        rows={[row("ram", [idle])]}
        unavailable={[
          { deployments: [ref("dark", "Dark GPU model")], kind: "vram", machine: gpu1.machine },
        ]}
      />,
    );

    const bar = screen.getByRole("img");
    expect(bar.children).toHaveLength(1);
    expect(bar).toHaveAttribute(
      "aria-label",
      "RAM on gpu1: 9 GiB used of 62 GiB; deployments: Idle 0 bytes; unreadable: gpu1 VRAM (Dark GPU model)",
    );
  });

  it("grays other models and reads out the focused deployment's own usage", () => {
    render(
      <MemoryRows
        focusDeploymentId="gemma"
        rows={[row("vram", [gpu1]), row("ram", [gpu1Host])]}
        surface="panel"
      />,
    );

    const [vram] = screen.getAllByRole("img");
    expect([...vram.children].map((child) => child.className.includes("grayscale"))).toEqual([
      false,
      true,
      false,
    ]);
    expect(screen.getByRole("button", { name: "VRAM 10 / 32 GiB" })).toBeVisible();
    expect(
      screen.getByRole("button", {
        name: "RAM — / 62 GiB, this model: host memory isn't estimated or measured",
      }),
    ).toBeVisible();
  });

  it("explains estimates, unmeasured models, and unreadable machines on hover", async () => {
    const user = userEvent.setup();
    const withUnmeasured: MemorySpan = {
      ...gpu1,
      unmeasured: [
        { ...ref("e4b", "Gemma 4 E4B QAT"), reason: "noEstimate" },
        { ...ref("old", "Old model"), reason: "stale" },
      ],
    };
    render(
      <MemoryRows
        rows={[row("vram", [withUnmeasured])]}
        unavailable={[
          {
            deployments: [ref("lost", "Lost model")],
            machine: { machineId: "rig", name: "rig-02" },
          },
        ]}
      />,
    );

    await user.hover(screen.getByRole("button", { name: "VRAM 28 / 32 GiB" }));

    const tooltip = await screen.findByText("VRAM: dedicated GPU memory");
    const content = tooltip.parentElement!;
    const lines = within(content)
      .getAllByText(/./)
      .map((line) => line.textContent);
    expect(lines.slice(0, -1)).toEqual([
      "VRAM: dedicated GPU memory",
      "28 GiB used · 4 GiB free",
      "Estimated: Qwen3.8 27B",
      "Not measured: Gemma 4 E4B QAT (no catalog sizing)",
      "Not measured: Old model (no current reading)",
      "Unreadable: rig-02 (Lost model)",
    ]);
    expect(lines.at(-1)).toMatch(/^Observed \d+ seconds? ago$/);
  });

  it("reads a unified pool as system memory and names each measurement gap", async () => {
    const user = userEvent.setup();
    const { rerender } = render(
      <MemoryRows focusDeploymentId="gemma" rows={[row("ram", [macHost])]} surface="panel" />,
    );

    await user.hover(screen.getByRole("button", { name: "RAM 5 / 36 GiB" }));

    const tooltip = await screen.findByText("RAM: system memory");
    const lines = within(tooltip.parentElement!)
      .getAllByText(/./)
      .map((line) => line.textContent);
    expect(lines.slice(0, -1)).toEqual([
      "This model: 5 GiB",
      "RAM: system memory",
      "25 GiB used · 11 GiB free",
      "Not measured: Gemma 4 E2B (waiting for the first reading)",
      "Not measured: Exited model (process not running)",
      "Not measured: Unread model (measurement unavailable)",
    ]);

    rerender(<MemoryRows focusDeploymentId="e2b" rows={[row("ram", [macHost])]} surface="panel" />);
    expect(
      screen.getByRole("button", {
        name: "RAM — / 36 GiB, this model: not measured, waiting for the first reading",
      }),
    ).toBeVisible();
    rerender(
      <MemoryRows focusDeploymentId="exited" rows={[row("ram", [macHost])]} surface="panel" />,
    );
    expect(
      screen.getByRole("button", {
        name: "RAM — / 36 GiB, this model: not measured, process not running",
      }),
    ).toBeVisible();
  });

  it("links a hovered segment to the shared hover state", async () => {
    const user = userEvent.setup();
    render(
      <MemoryHoverProvider>
        <MemoryRows rows={[row("vram", [gpu1])]} />
      </MemoryHoverProvider>,
    );

    const [gemma, qwen] = [...screen.getByRole("img").children];
    await user.hover(gemma);

    expect(gemma.className).not.toContain("grayscale");
    expect(qwen.className).toContain("grayscale");
    expect(await screen.findByText("gpu1 · 10 GiB")).toBeVisible();
  });

  it("leaves the shared hover state alone when an application segment is hovered", async () => {
    const user = userEvent.setup();
    const comfy = { ...segment("app-comfy", "ComfyUI", 2n), owner: "application" as const };
    render(
      <MemoryHoverProvider>
        <MemoryRows
          rows={[row("vram", [span("gpu1", 32n, [segment("gemma", "Gemma", 10n), comfy], 0n)])]}
        />
      </MemoryHoverProvider>,
    );

    const [gemma, app] = [...screen.getByRole("img").children];
    await user.hover(app);

    expect(gemma.className).not.toContain("grayscale");
    expect(app.className).not.toContain("grayscale");
  });

  it("opens a row's breakdown on a click anywhere on the row, and closes it outside", async () => {
    const user = userEvent.setup();
    render(
      <>
        <MemoryRows rows={[row("vram", [gpu1]), row("ram", [gpu1Host])]} />
        <p>Outside</p>
      </>,
    );

    const [gemma] = [...screen.getAllByRole("img")[0].children];
    await user.click(gemma);

    const breakdown = await screen.findByRole("dialog", { name: "VRAM memory breakdown" });
    expect(
      [...breakdown.children].map((part) =>
        within(part as HTMLElement)
          .queryAllByText(/./)
          .map((line) => line.textContent),
      ),
    ).toEqual([
      [
        "VRAM: dedicated GPU memory",
        "28 GiB used · 4 GiB free",
        "Estimated: Qwen3.8 27B",
        expect.stringMatching(/^Observed \d+ seconds? ago$/),
      ],
      ["Gemma 4 12B QAT", "gpu1 · 10 GiB"],
      ["Qwen3.8 27B", "gpu1 · 17 GiB (estimated)"],
      ["Other", "gpu1 · 1 GiB"],
    ]);

    await user.click(screen.getByText("Outside"));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());

    await user.click(screen.getByText("RAM"));
    expect(await screen.findByRole("dialog", { name: "RAM memory breakdown" })).toHaveTextContent(
      /^RAM: system memory/,
    );

    await user.click(screen.getByText("RAM"));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  });

  it("opens a row's breakdown from its readout by keyboard, keeping its label", async () => {
    const user = userEvent.setup();
    render(<MemoryRows rows={[row("vram", [gpu1])]} />);

    await user.tab();
    const readout = screen.getByRole("button", { name: "VRAM 28 / 32 GiB" });
    expect(readout).toHaveFocus();
    expect(readout).toHaveAccessibleDescription(
      /^VRAM: dedicated GPU memory\. 28 GiB used · 4 GiB free\. Estimated: Qwen3\.8 27B\./,
    );

    await user.keyboard("{Enter}");

    expect(await screen.findByRole("dialog")).toHaveTextContent(/^VRAM: dedicated GPU memory/);
    expect(readout).toHaveAttribute("aria-expanded", "true");

    await user.keyboard("{Escape}");

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  });
});

describe("MemoryMetricRows", () => {
  it("expands a row on a click to its body, leaving the breakdown to hover", async () => {
    const user = userEvent.setup();
    render(<ExpandableRows />);

    const [gemma] = [...screen.getAllByRole("img")[0].children];
    await user.click(gemma);

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "VRAM" })).toHaveAttribute("aria-expanded", "true");
    expect(screen.queryByText("VRAM: dedicated GPU memory")).not.toBeInTheDocument();
    expect(screen.getByText("vram history")).toBeVisible();
    expect(screen.queryByText("ram history")).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "VRAM" }));
    expect(screen.queryByText("vram history")).not.toBeInTheDocument();
  });

  it("leaves a row collapsed when its readout is pressed by keyboard", async () => {
    const user = userEvent.setup();
    render(<ExpandableRows />);

    screen.getByRole("button", { name: "VRAM 28 / 32 GiB" }).focus();
    await user.keyboard("{Enter}");

    expect(screen.getByRole("button", { name: "VRAM" })).toHaveAttribute("aria-expanded", "false");
  });

  it("keeps the readout's detail tooltip and the bar's label", async () => {
    const user = userEvent.setup();
    render(<ExpandableRows />);

    expect(screen.getAllByRole("img")[0]).toHaveAttribute(
      "aria-label",
      "VRAM on gpu1: 28 GiB used of 32 GiB; deployments: Gemma 4 12B QAT 10 GiB, Qwen3.8 27B 17 GiB; estimated: Qwen3.8 27B",
    );
    await user.hover(screen.getByRole("button", { name: "VRAM 28 / 32 GiB" }));

    expect(await screen.findByText("28 GiB used · 4 GiB free")).toBeVisible();
  });

  it("marks a metric row past its total, merged at the row's total or per machine past its own", () => {
    const capacity = { ram: beyondTotal(10n * gib, 240n * gib) };
    const overOne = {
      ...span("spark-01", 120n, [segment("big", "Big model", 125n)], 5n),
      overTotalBytes: 10n * gib,
    };
    const rows = [row("ram", [overOne, sparkTwo])];
    const { container, rerender } = render(
      <MetricRows>
        <MemoryMetricRows
          capacity={capacity}
          expanded={() => false}
          onExpandedChange={() => {}}
          rows={rows}
        >
          {() => null}
        </MemoryMetricRows>
      </MetricRows>,
    );

    const markers = () =>
      [...container.querySelectorAll<HTMLElement>('[data-slot="memory-capacity-marker"]')].map(
        (marker) => [marker.dataset.line, marker.style.left],
      );
    // Merged, the other machine's free memory keeps drawn use under the row's total.
    expect(markers()).toEqual([["total", "100%"]]);
    const readout = screen.getByRole("button", {
      name: "RAM 160 / 240 GiB, over capacity by 10 GiB",
    });
    expect(readout).toHaveTextContent(/^▲ 160 \/ 240 GiB$/);
    expect(readout.className).toContain("text-destructive");

    rerender(
      <MetricRows>
        <MemoryMetricRows
          capacity={capacity}
          expanded={() => false}
          mode="spans"
          onExpandedChange={() => {}}
          rows={rows}
        >
          {() => null}
        </MemoryMetricRows>
      </MetricRows>,
    );

    expect(markers()).toEqual([["total", `${(120 / 130) * 100}%`]]);
    expect(screen.getAllByRole("img").map((bar) => bar.getAttribute("aria-label"))).toEqual([
      "RAM on spark-01: 130 GiB used of 120 GiB; deployments: Big model 125 GiB; over capacity by 10 GiB",
      "RAM on spark-02: 30 GiB used of 120 GiB",
    ]);
  });

  it("keeps each machine's span in spans mode and still expands", async () => {
    const user = userEvent.setup();
    render(<ExpandableRows mode="spans" rows={[row("ram", [sparkOne, sparkTwo])]} />);

    expect(screen.getAllByRole("img").map((bar) => bar.getAttribute("aria-label"))).toEqual([
      "RAM on spark-01: 50 GiB used of 120 GiB; deployments: Qwen3.8 27B 40 GiB",
      "RAM on spark-02: 30 GiB used of 120 GiB",
    ]);
    expect(
      [...document.querySelectorAll<HTMLElement>('[data-slot="metric-span"]')].map(
        (part) => part.style.flex,
      ),
    ).toEqual([`${Number(120n * gib)} 1 0%`, `${Number(120n * gib)} 1 0%`]);
    expectResourceMarker(screen.getByText("spark-01"), "spark-01");
    expect(screen.getByRole("button", { name: "RAM 80 / 240 GiB" })).toBeVisible();

    await user.click(screen.getByRole("button", { name: "RAM" }));
    expect(screen.getByText("ram history")).toBeVisible();
  });
});

/** Memory metric rows holding their own expansion, with a named body per row. */
function ExpandableRows({
  mode,
  rows = [row("vram", [gpu1]), row("ram", [gpu1Host])],
}: {
  mode?: "merged" | "spans";
  rows?: MemoryRow[];
}) {
  const [open, setOpen] = useState<ReadonlySet<string>>(new Set());

  return (
    <MetricRows>
      <MemoryMetricRows
        expanded={(kind) => open.has(kind)}
        onExpandedChange={(kind, expanded) =>
          setOpen((current) => {
            const next = new Set(current);
            if (expanded) next.add(kind);
            else next.delete(kind);
            return next;
          })
        }
        mode={mode}
        rows={rows}
      >
        {(memoryRow) => <p>{memoryRow.kind} history</p>}
      </MemoryMetricRows>
    </MetricRows>
  );
}

const gpu1 = span(
  "gpu1",
  32n,
  [segment("gemma", "Gemma 4 12B QAT", 10n), segment("qwen", "Qwen3.8 27B", 17n, true)],
  1n,
);
const gpu1Host = span("gpu1", 62n, [], 9n);
const sparkOne = span("spark-01", 120n, [segment("qwen-spark", "Qwen3.8 27B", 40n)], 10n);
const sparkTwo = span("spark-02", 120n, [], 30n);
const macHost: MemorySpan = {
  ...span("mac-host", 36n, [segment("gemma", "Gemma 4 12B", 5n)], 20n),
  unmeasured: [
    { ...ref("e2b", "Gemma 4 E2B"), reason: "pending" },
    { ...ref("exited", "Exited model"), reason: "notRunning" },
    { ...ref("unread", "Unread model"), reason: "unavailable" },
  ],
};

function row(kind: MemoryRow["kind"], spans: MemorySpan[]): MemoryRow {
  return {
    kind,
    spans: spans.map((item) => ({ ...item, kind })),
    totalBytes: spans.reduce((total, item) => total + item.totalBytes, 0n),
    usedBytes: spans.reduce((total, item) => total + item.usedBytes, 0n),
  };
}

/** A machine's span whose drawn use is its segments and other use. */
function span(
  name: string,
  totalGib: bigint,
  segments: MemorySegment[],
  otherGib: bigint,
): MemorySpan {
  return {
    devices: [],
    kind: "vram",
    machine: { machineId: name, name },
    observedAt: {
      $typeName: "google.protobuf.Timestamp",
      nanos: 0,
      seconds: BigInt(Math.floor(Date.now() / 1000) - 1),
    },
    otherBytes: otherGib * gib,
    otherLabel: "Other",
    overTotalBytes: 0n,
    segments: segments.map((item) => ({ ...item, machineId: name })),
    stale: false,
    totalBytes: totalGib * gib,
    unmeasured: [],
    usedBytes: segments.reduce((total, item) => total + item.bytes, otherGib * gib),
  };
}

function segment(id: string, name: string, bytesGib: bigint, estimated = false): MemorySegment {
  return {
    ...ref(id, name),
    bytes: bytesGib * gib,
    machineId: "",
    source: estimated ? "estimated" : "observed",
  };
}

function predictedSegment(id: string, name: string, bytesGib: bigint): MemorySegment {
  return { ...segment(id, name, bytesGib), source: "predicted" };
}

/** A verdict that puts a row's use `overBytes` beyond its `totalBytes`. */
function beyondTotal(overBytes: bigint, totalBytes: bigint): MemoryRowCapacity {
  return { beyondTotal: true, headroomPercent: 1, lineBytes: totalBytes, overBytes };
}

function ref(id: string, name: string) {
  return { colorKey: id, deploymentId: id, name };
}

/** The computed form a browser gives an inline hex color. */
function cssColor(hex: string): string {
  const element = document.createElement("span");
  element.style.backgroundColor = hex;
  return element.style.backgroundColor;
}
