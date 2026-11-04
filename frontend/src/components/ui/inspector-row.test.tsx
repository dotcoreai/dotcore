import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { Badge } from "@/components/ui/badge";
import { InspectorRow, InspectorRowList } from "@/components/ui/inspector-row";
import { ResourceMarker } from "@/components/ui/resource-marker";

describe("InspectorRow", () => {
  it("shows its label and trailing content inside the labeled list", () => {
    render(
      <InspectorRowList ariaLabel="Sub-screens">
        <InspectorRow
          accessibleLabel="Open Configuration"
          label="Configuration"
          onSelect={vi.fn()}
          trailing={<Badge variant="neutral">Default</Badge>}
        />
      </InspectorRowList>,
    );

    const row = screen.getByRole("button", { name: "Open Configuration" });
    expect(row).toHaveTextContent("Configuration");
    expect(row).toHaveTextContent("Default");
    expect(screen.getByRole("list", { name: "Sub-screens" })).toContainElement(row);
  });

  it("renders only its label when no trailing content is given", () => {
    render(
      <InspectorRowList ariaLabel="Sub-screens">
        <InspectorRow
          accessibleLabel="Open Configuration"
          label="Configuration"
          onSelect={vi.fn()}
        />
      </InspectorRowList>,
    );

    expect(screen.getByRole("button", { name: "Open Configuration" })).toHaveTextContent(
      /^Configuration$/,
    );
  });

  it("shows a description line under the label", () => {
    render(
      <InspectorRowList ariaLabel="Profiles">
        <InspectorRow
          accessibleLabel="View profile Fast"
          description="llama.cpp · Q4_K_M"
          label="Fast"
          onSelect={vi.fn()}
        />
      </InspectorRowList>,
    );

    const row = screen.getByRole("button", { name: "View profile Fast" });
    expect(row).toHaveTextContent(/^Fastllama\.cpp · Q4_K_M$/);
    expect(screen.getByText("llama.cpp · Q4_K_M")).toHaveClass("text-muted-foreground");
  });

  it("renders a static row with no control and no chevron when no handler is given", () => {
    render(
      <InspectorRowList ariaLabel="Members">
        <InspectorRow label="gemma-4-12b-it-qat" trailing="Eligible" />
      </InspectorRowList>,
    );

    const list = screen.getByRole("list", { name: "Members" });
    expect(list).toHaveTextContent("gemma-4-12b-it-qat");
    expect(list).toHaveTextContent("Eligible");
    expect(screen.queryByRole("button")).not.toBeInTheDocument();
    expect(list.querySelector("svg")).toBeNull();
  });

  it("carries an identity marker before a static row label", () => {
    render(
      <InspectorRowList ariaLabel="Members">
        <InspectorRow
          label={
            <>
              <ResourceMarker colorKey="deployment-2" />
              gemma-4-12b-it-qat
            </>
          }
        />
      </InspectorRowList>,
    );

    const label = screen.getByText("gemma-4-12b-it-qat", { selector: "span" });
    expect(label).toHaveClass("truncate");
    const marker = label.querySelector("[data-slot=resource-marker]");
    expect(marker).not.toBeNull();
    expect(marker).toHaveAttribute("aria-hidden", "true");
  });

  it("fires on click and on keyboard activation from tab focus", async () => {
    const user = userEvent.setup();
    const onSelect = vi.fn();
    render(
      <InspectorRowList ariaLabel="Sub-screens">
        <InspectorRow accessibleLabel="Open Details" label="Details" onSelect={onSelect} />
      </InspectorRowList>,
    );

    const row = screen.getByRole("button", { name: "Open Details" });
    await user.tab();
    expect(row).toHaveFocus();
    await user.keyboard("{Enter}");
    expect(onSelect).toHaveBeenCalledTimes(1);

    await user.click(row);
    expect(onSelect).toHaveBeenCalledTimes(2);
  });
});
