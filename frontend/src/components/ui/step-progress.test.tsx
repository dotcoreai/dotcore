import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { StepProgress } from "@/components/ui/step-progress";

const STEPS = ["Check", "Download", "Prepare", "Load"];

function segmentStates(bar: HTMLElement) {
  return [...bar.querySelectorAll('[data-slot="step-progress-segment"]')].map((segment) =>
    segment.getAttribute("data-state"),
  );
}

describe("StepProgress", () => {
  it("fills done segments, marks the current one and leaves later ones empty", () => {
    render(<StepProgress current={2} steps={STEPS} />);

    const bar = screen.getByRole("progressbar", { name: "Prepare" });
    expect(segmentStates(bar)).toEqual(["done", "done", "current", "pending"]);
    expect(bar).toHaveTextContent("Step 3 of 4");
    expect(bar).toHaveAttribute("aria-valuenow", "2");
    expect(bar).toHaveAttribute("aria-valuemax", "4");
    expect(bar).toHaveAttribute("aria-valuetext", "Prepare, Step 3 of 4");
  });

  it("slides a block in the current segment when nothing is measurable", () => {
    render(<StepProgress current={0} steps={STEPS} />);

    const bar = screen.getByRole("progressbar", { name: "Check" });
    const block = bar.querySelector('[data-slot="step-progress-block"]');
    expect(block).toHaveClass("animate-slide-block", "motion-reduce:animate-none");
    expect(bar.querySelectorAll('[data-slot="step-progress-block"]')).toHaveLength(1);
    expect(segmentStates(bar)).toEqual(["current", "pending", "pending", "pending"]);
  });

  it("fills the current segment with the measured percent and appends its text", () => {
    render(
      <StepProgress
        current={1}
        currentProgress={{ percent: 40, text: "40 MiB / 100 MiB · 40%" }}
        steps={STEPS}
      />,
    );

    const bar = screen.getByRole("progressbar", { name: "Download" });
    expect(bar).toHaveTextContent("Step 2 of 4 · 40 MiB / 100 MiB · 40%");
    expect(bar.querySelector('[data-slot="step-progress-fill"]')).toHaveStyle({ width: "40%" });
    expect(bar.querySelector('[data-slot="step-progress-block"]')).toBeNull();
  });

  it("slides and shows the text when the measured step has no known total", () => {
    render(
      <StepProgress current={1} currentProgress={{ text: "1.5 KiB downloaded" }} steps={STEPS} />,
    );

    const bar = screen.getByRole("progressbar", { name: "Download" });
    expect(bar).toHaveTextContent("Step 2 of 4 · 1.5 KiB downloaded");
    expect(bar.querySelector('[data-slot="step-progress-fill"]')).toBeNull();
    expect(bar.querySelector('[data-slot="step-progress-block"]')).not.toBeNull();
  });

  it("shows every segment done-but-last on the final step", () => {
    render(<StepProgress current={3} steps={STEPS} />);

    expect(segmentStates(screen.getByRole("progressbar", { name: "Load" }))).toEqual([
      "done",
      "done",
      "done",
      "current",
    ]);
  });
});
