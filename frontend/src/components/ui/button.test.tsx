import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { Button } from "@/components/ui/button";

describe("Button", () => {
  it("matches the demo's console control geometry at size=console", () => {
    render(<Button size="console">Load</Button>);

    const button = screen.getByRole("button", { name: "Load" });
    expect(button).toHaveClass("min-h-control-action");
    expect(button).toHaveClass("rounded-[min(var(--radius-md),8px)]");
    expect(button).toHaveClass("px-3");
    expect(button).toHaveClass("py-1.5");
    expect(button).toHaveClass("text-[12.5px]");
  });

  it("gives compact sizes a tap area, letting a caller's position replace relative", () => {
    render(
      <>
        <Button size="xs">Xs</Button>
        <Button size="sm">Sm</Button>
        <Button size="icon-xs" aria-label="Icon xs" />
        <Button size="icon-sm" aria-label="Icon sm" className="absolute top-2 right-2" />
      </>,
    );

    expect(screen.getByRole("button", { name: "Xs" })).toHaveClass(
      "h-control-xs",
      "relative",
      "touch-target",
    );
    expect(screen.getByRole("button", { name: "Sm" })).toHaveClass(
      "h-control-sm",
      "relative",
      "touch-target",
    );
    expect(screen.getByRole("button", { name: "Icon xs" })).toHaveClass(
      "size-control-xs",
      "relative",
      "touch-target",
    );

    const positioned = screen.getByRole("button", { name: "Icon sm" });
    expect(positioned).toHaveClass("size-control-sm", "absolute", "touch-target");
    expect(positioned).not.toHaveClass("relative");
  });

  it("renders the primary fill the demo's .btn-primary uses", () => {
    render(<Button variant="default">Load</Button>);

    const button = screen.getByRole("button", { name: "Load" });
    expect(button).toHaveClass("bg-primary");
    expect(button).toHaveClass("text-primary-foreground");
  });

  it("renders the danger treatment as an outline, not a permanent fill", () => {
    render(<Button variant="destructive">Unload</Button>);

    const button = screen.getByRole("button", { name: "Unload" });
    expect(button).toHaveClass("border-destructive/40");
    expect(button).toHaveClass("text-destructive");
    expect(button).toHaveClass("hover:bg-destructive/10");
    expect(button.className).not.toMatch(/(?<!hover:)(?<!dark:)bg-destructive(?!\/)/);
  });

  it("keeps a visible focus ring on both the primary and danger treatments", () => {
    render(
      <>
        <Button variant="default">Load</Button>
        <Button variant="destructive">Unload</Button>
      </>,
    );

    for (const button of screen.getAllByRole("button")) {
      expect(button.className).toMatch(/focus-visible:ring-/);
    }
  });
});
