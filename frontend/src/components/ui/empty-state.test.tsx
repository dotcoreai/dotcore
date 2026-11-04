import { render, screen, within } from "@testing-library/react";
import { Boxes } from "lucide-react";
import { describe, expect, it } from "vitest";

import { EmptyState } from "@/components/ui/empty-state";

describe("EmptyState", () => {
  it("renders the title, description and action as one status region", () => {
    render(
      <EmptyState
        action={<button type="button">Deploy a model</button>}
        description="Deploy a model from the catalog."
        icon={Boxes}
        title="No models running"
      />,
    );

    const status = screen.getByRole("status");
    expect(within(status).getByRole("heading", { name: "No models running" })).toBeVisible();
    expect(within(status).getByText("Deploy a model from the catalog.")).toBeVisible();
    expect(within(status).getByRole("button", { name: "Deploy a model" })).toBeVisible();
  });

  it("hides the icon and the placeholder cards from assistive technology", () => {
    const { container } = render(
      <EmptyState action={null} description="Help" icon={Boxes} title="Title" />,
    );

    expect(container.querySelectorAll("svg[aria-hidden='true']")).toHaveLength(1);
    const placeholders = container.querySelector(".inspector-card-grid");
    expect(placeholders).toHaveAttribute("aria-hidden", "true");
    expect(placeholders?.children).toHaveLength(4);
  });
});
