import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { Button } from "@/components/ui/button";
import { InspectorSectionHeader } from "@/components/ui/inspector-section-header";

describe("InspectorSectionHeader", () => {
  it("renders a level-2 heading carrying the given id", () => {
    render(<InspectorSectionHeader headingId="deployment-heading" title="Deployment" />);

    const heading = screen.getByRole("heading", { level: 2, name: "Deployment" });
    expect(heading).toHaveAttribute("id", "deployment-heading");
  });

  it("renders nothing beside the heading when given neither a meta value nor an action", () => {
    render(<InspectorSectionHeader headingId="deployment-heading" title="Deployment" />);

    expect(screen.getByRole("heading", { name: "Deployment" }).parentElement).toHaveTextContent(
      /^Deployment$/,
    );
  });

  it("names the section it labels and shows its meta value", () => {
    render(
      <section aria-labelledby="profiles-heading">
        <InspectorSectionHeader headingId="profiles-heading" meta="4 profiles" title="Profiles" />
      </section>,
    );

    expect(screen.getByRole("region", { name: "Profiles" })).toHaveTextContent("4 profiles");
  });

  it("renders a meta node and keeps its title attribute", () => {
    render(
      <section aria-labelledby="hardware-heading">
        <InspectorSectionHeader
          headingId="hardware-heading"
          meta={<span title="2026-09-16 18:35:00 UTC">Observed 2 minutes ago</span>}
          title="Hardware"
        />
      </section>,
    );

    const meta = screen.getByText("Observed 2 minutes ago");
    expect(meta).toHaveAttribute("title", "2026-09-16 18:35:00 UTC");
    expect(screen.getByRole("region", { name: "Hardware" })).toContainElement(meta);
  });

  it("fires its action on click and on keyboard activation from tab focus", async () => {
    const user = userEvent.setup();
    const onSelect = vi.fn();
    render(
      <InspectorSectionHeader
        action={
          <Button onClick={onSelect} size="sm" type="button" variant="link">
            Logs
          </Button>
        }
        headingId="status-heading"
        title="Status"
      />,
    );

    const action = screen.getByRole("button", { name: "Logs" });
    await user.tab();
    expect(action).toHaveFocus();
    await user.keyboard("{Enter}");
    expect(onSelect).toHaveBeenCalledTimes(1);

    await user.click(action);
    expect(onSelect).toHaveBeenCalledTimes(2);
  });
});
