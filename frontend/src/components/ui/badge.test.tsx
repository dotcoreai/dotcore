import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { Badge } from "@/components/ui/badge";

describe("Badge", () => {
  it.each([
    ["success", "bg-success", "text-success-foreground"],
    ["info", "bg-info", "text-primary"],
    ["warning", "bg-warning", "text-warning-foreground"],
    ["neutral", "bg-muted", "text-muted-foreground"],
    ["destructive", "bg-danger", "text-destructive"],
  ] as const)("renders the %s tone with the demo's chip and ink classes", (variant, bg, fg) => {
    render(<Badge variant={variant}>Label</Badge>);

    const badge = screen.getByText("Label");
    expect(badge).toHaveClass(bg);
    expect(badge).toHaveClass(fg);
  });

  it("matches the demo's pill geometry regardless of tone", () => {
    render(<Badge variant="success">Ready</Badge>);

    const badge = screen.getByText("Ready");
    expect(badge).toHaveClass("rounded-full");
    expect(badge).toHaveClass("px-[9px]");
    expect(badge).toHaveClass("py-[3px]");
    expect(badge).toHaveClass("text-[11px]");
    expect(badge).toHaveClass("font-semibold");
    expect(badge).toHaveClass("w-fit");
  });
});
