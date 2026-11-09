import { render, screen } from "@testing-library/react";
import { Boxes } from "lucide-react";
import { describe, expect, it } from "vitest";

import {
  PageActions,
  PageHeader,
  PageHeaderDescription,
  PageHeaderHeading,
  PageSectionHeading,
} from "@/components/ui/page-header";

describe("PageHeader", () => {
  it("renders a semantic heading with optional description and actions", () => {
    render(
      <PageHeader>
        <div>
          <PageHeaderHeading>Routes</PageHeaderHeading>
          <PageHeaderDescription>Manage inference routes.</PageHeaderDescription>
        </div>
        <PageActions>
          <button type="button">Create route</button>
        </PageActions>
      </PageHeader>,
    );

    expect(screen.getByRole("heading", { level: 1, name: "Routes" })).toHaveAttribute(
      "data-slot",
      "page-header-heading",
    );
    expect(screen.getByText("Manage inference routes.")).toHaveAttribute(
      "data-slot",
      "page-header-description",
    );
    expect(screen.getByRole("button", { name: "Create route" }).parentElement).toHaveAttribute(
      "data-slot",
      "page-actions",
    );
  });

  it("keeps page actions on the heading's row, wrapping them below only when they don't fit", () => {
    render(
      <PageHeader data-testid="header">
        <PageHeaderHeading>Routes</PageHeaderHeading>
        <PageActions data-testid="actions">Create route</PageActions>
      </PageHeader>,
    );

    const header = screen.getByTestId("header");
    expect(header).toHaveClass(
      "flex",
      "min-h-control-action",
      "items-center",
      "justify-between",
      "flex-wrap",
      "gap-x-4",
      "gap-y-3",
    );
    // No breakpoint may stack the header: a stacked header is taller than one without actions even
    // when its actions fit, which is the offset this rule exists to prevent. Wrapping only moves
    // actions that overflow the row.
    expect(header.className).not.toMatch(/flex-col|:flex-row/);
    expect(screen.getByTestId("actions")).toHaveClass("flex", "flex-wrap", "items-center", "gap-2");
  });

  it("renders a decorative icon before the heading text", () => {
    render(<PageHeaderHeading icon={Boxes}>Models</PageHeaderHeading>);

    const heading = screen.getByRole("heading", { level: 1, name: "Models" });
    const icon = heading.querySelector("svg");
    expect(heading.firstElementChild).toBe(icon);
    expect(icon).toHaveAttribute("aria-hidden", "true");
    expect(icon).toHaveClass("lucide-boxes", "size-5", "shrink-0", "text-muted-foreground");
  });

  it("renders no icon when none is provided", () => {
    render(<PageHeaderHeading>Routes</PageHeaderHeading>);

    expect(
      screen.getByRole("heading", { level: 1, name: "Routes" }).querySelector("svg"),
    ).toBeNull();
  });

  it("renders a page section heading one level below the page heading", () => {
    render(
      <PageSectionHeading className="text-primary" id="running-heading">
        Running
      </PageSectionHeading>,
    );

    const heading = screen.getByRole("heading", { level: 2, name: "Running" });
    expect(heading).toHaveAttribute("data-slot", "page-section-heading");
    expect(heading).toHaveAttribute("id", "running-heading");
    expect(heading).toHaveClass("text-[17px]", "leading-6", "font-semibold", "text-primary");
  });

  it("forwards props and allows token-based class customization", () => {
    render(
      <PageHeader data-testid="header" aria-label="Route management" className="bg-card">
        <PageHeaderHeading className="text-primary">Routes</PageHeaderHeading>
      </PageHeader>,
    );

    expect(screen.getByTestId("header")).toHaveAttribute("aria-label", "Route management");
    expect(screen.getByRole("heading", { name: "Routes" })).toHaveClass("text-primary");
    expect(screen.getByTestId("header")).toHaveClass("bg-card");
  });
});
