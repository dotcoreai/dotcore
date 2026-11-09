import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";

import { expectResourceMarker } from "@/test/resource-marker";
import { getResourceColor } from "@/theme/resource-color";

import { MetricBar, MetricRow, MetricRows, MetricSpans } from "./metric-row";

describe("MetricRow", () => {
  it("toggles its body from the button with pointer, Enter and Space", async () => {
    const user = userEvent.setup();
    render(<Row />);

    const toggle = screen.getByRole("button", { name: "CPU" });
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByText("CPU history")).not.toBeInTheDocument();

    await user.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByText("CPU history")).toBeVisible();
    expect(toggle.getAttribute("aria-controls")).toBe(
      screen.getByText("CPU history").closest('[data-slot="metric-row-body"]')?.id,
    );

    await user.keyboard("{Enter}");
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    await user.keyboard(" ");
    expect(toggle).toHaveAttribute("aria-expanded", "true");
  });

  it("toggles from a click on the bar, beside the button", async () => {
    const user = userEvent.setup();
    render(<Row />);

    await user.click(document.querySelector('[data-slot="metric-bar"]')!);

    expect(screen.getByRole("button", { name: "CPU" })).toHaveAttribute("aria-expanded", "true");
  });

  it("names the toggle by its label and keeps the value as text", () => {
    render(<Row label="/" />);

    expect(screen.getByRole("button", { name: "/" })).toBeVisible();
    expect(screen.getByText("34%")).toBeVisible();
  });

  it("is a plain line without an expanded body", async () => {
    const user = userEvent.setup();
    const onExpandedChange = vi.fn();
    render(
      <MetricRows>
        <MetricRow
          bar={<MetricBar fraction={0.34} />}
          expanded={false}
          label="RAM"
          onExpandedChange={onExpandedChange}
          value="34%"
        />
      </MetricRows>,
    );

    await user.click(document.querySelector('[data-slot="metric-bar"]')!);

    expect(screen.queryByRole("button")).not.toBeInTheDocument();
    expect(screen.getByText("RAM")).toBeVisible();
    expect(onExpandedChange).not.toHaveBeenCalled();
  });
});

describe("MetricBar", () => {
  it("fills its share, clamped, and stays hidden from assistive technology", () => {
    const { rerender } = render(<MetricBar fraction={0.34} />);
    const bar = document.querySelector('[data-slot="metric-bar"]');
    expect(bar).toHaveAttribute("aria-hidden", "true");
    expect(document.querySelector<HTMLElement>('[data-slot="metric-bar-fill"]')?.style.width).toBe(
      "34%",
    );

    rerender(<MetricBar fraction={1.5} />);
    expect(document.querySelector<HTMLElement>('[data-slot="metric-bar-fill"]')?.style.width).toBe(
      "100%",
    );

    rerender(<MetricBar fraction={undefined} />);
    expect(document.querySelector('[data-slot="metric-bar-fill"]')).toBeNull();
  });

  it("fills with a token or with a resource's marker color", () => {
    const fill = () => document.querySelector<HTMLElement>('[data-slot="metric-bar-fill"]');
    const { rerender } = render(<MetricBar color="chart-4" fraction={0.5} />);
    expect(fill()?.style.backgroundColor).toBe("var(--chart-4)");

    rerender(<MetricBar color={{ resource: "node-a" }} fraction={0.5} />);
    expect(fill()).toHaveStyle({ backgroundColor: getResourceColor("node-a") });
  });
});

describe("MetricSpans", () => {
  it("sizes each part by its weight and names it with its marker and value", () => {
    render(
      <MetricSpans
        spans={[
          {
            bar: <MetricBar fraction={0.12} />,
            colorKey: "node-a",
            key: "a",
            name: "gpu1",
            value: "12%",
            weight: 32,
          },
          {
            bar: <MetricBar fraction={0.46} />,
            colorKey: "node-b",
            key: "b",
            name: "spark-01",
            value: "46%",
            weight: 20,
          },
        ]}
      />,
    );

    const parts = document.querySelectorAll<HTMLElement>('[data-slot="metric-span"]');
    expect([...parts].map((part) => part.style.flex)).toEqual(["32 1 0%", "20 1 0%"]);
    const first = screen.getByText(/gpu1/);
    expect(first).toHaveTextContent("gpu1 12%");
    expectResourceMarker(first, "node-a");
    expectResourceMarker(screen.getByText(/spark-01/), "node-b");
  });
});

function Row({ label = "CPU" }: { label?: string }) {
  const [expanded, setExpanded] = useState(false);

  return (
    <MetricRows>
      <MetricRow
        bar={<MetricBar fraction={0.34} />}
        expanded={expanded}
        label={label}
        onExpandedChange={setExpanded}
        value="34%"
      >
        <p>CPU history</p>
      </MetricRow>
    </MetricRows>
  );
}
