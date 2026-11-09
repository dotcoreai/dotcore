import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { ResourceMarker, ResourceTitle } from "@/components/ui/resource-marker";
import { getResourceColor } from "@/theme/resource-color";

describe("ResourceMarker", () => {
  it("renders a decorative marker filled with the key's color", () => {
    const { container } = render(<ResourceMarker colorKey="route-alpha" />);

    const marker = container.querySelector('[data-slot="resource-marker"]');
    expect(marker).toHaveAttribute("aria-hidden", "true");
    expect(marker).toHaveClass("size-2.5", "rounded-[3px]");
    expect(marker).toHaveStyle({ backgroundColor: getResourceColor("route-alpha") });
  });
});

describe("ResourceTitle", () => {
  it("leads the resource name with its marker and keeps the accessible name text-only", () => {
    render(
      <h2>
        <ResourceTitle colorKey="machine-a" name="Machine A" />
      </h2>,
    );

    const heading = screen.getByRole("heading", { name: "Machine A" });
    const marker = heading.firstElementChild;
    expect(marker).toHaveAttribute("data-slot", "resource-marker");
    expect(marker).toHaveStyle({ backgroundColor: getResourceColor("machine-a") });
  });

  it("appends the screen after a middle dot", () => {
    render(
      <h2>
        <ResourceTitle colorKey="route-alpha" name="route-alpha" screen="Edit" />
      </h2>,
    );

    expect(screen.getByRole("heading", { name: "route-alpha · Edit" })).toBeInTheDocument();
  });
});
