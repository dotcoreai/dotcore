import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { Progress, ProgressLabel, ProgressValue } from "@/components/ui/progress";

describe("Progress", () => {
  it("exposes an indeterminate state without an invented numeric value", () => {
    render(
      <Progress value={null}>
        <ProgressLabel>Download progress</ProgressLabel>
        <ProgressValue>{() => "1.5 KiB downloaded"}</ProgressValue>
      </Progress>,
    );

    const progress = screen.getByRole("progressbar", { name: "Download progress" });
    expect(progress).toHaveAttribute("data-indeterminate");
    expect(progress).not.toHaveAttribute("aria-valuenow");
    expect(screen.getByText("1.5 KiB downloaded")).toBeVisible();
    expect(document.querySelector('[data-slot="progress-indicator"]')).toHaveClass(
      "data-[indeterminate]:animate-pulse",
      "motion-reduce:data-[indeterminate]:animate-none",
    );
  });
});
