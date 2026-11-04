import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { InspectorOpenerCard } from "@/components/ui/inspector-opener-card";

describe("InspectorOpenerCard", () => {
  it("renders data-selected when selected, and omits the attribute otherwise", () => {
    const { rerender } = render(
      <InspectorOpenerCard label="Open" onSelect={vi.fn()} panelId="panel" selected>
        Body
      </InspectorOpenerCard>,
    );
    expect(screen.getByRole("button", { name: "Open" })).toHaveAttribute("data-selected", "true");

    rerender(
      <InspectorOpenerCard label="Open" onSelect={vi.fn()} panelId="panel" selected={false}>
        Body
      </InspectorOpenerCard>,
    );
    expect(screen.getByRole("button", { name: "Open" })).not.toHaveAttribute("data-selected");
  });

  it("tracks aria-expanded from selected, and sets aria-controls to panelId", () => {
    const { rerender } = render(
      <InspectorOpenerCard label="Open" onSelect={vi.fn()} panelId="the-panel" selected={false}>
        Body
      </InspectorOpenerCard>,
    );
    const card = screen.getByRole("button", { name: "Open" });
    expect(card).toHaveAttribute("aria-expanded", "false");
    expect(card).toHaveAttribute("aria-controls", "the-panel");

    rerender(
      <InspectorOpenerCard label="Open" onSelect={vi.fn()} panelId="the-panel" selected>
        Body
      </InspectorOpenerCard>,
    );
    expect(card).toHaveAttribute("aria-expanded", "true");
  });

  it("fires onSelect with the card element on click, Enter and Space", () => {
    const onSelect = vi.fn();
    render(
      <InspectorOpenerCard label="Open" onSelect={onSelect} panelId="panel" selected={false}>
        Body
      </InspectorOpenerCard>,
    );
    const card = screen.getByRole("button", { name: "Open" });

    fireEvent.click(card);
    fireEvent.keyDown(card, { key: "Enter" });
    fireEvent.keyDown(card, { key: " " });

    expect(onSelect).toHaveBeenCalledTimes(3);
    for (const call of onSelect.mock.calls) {
      expect(call[0]).toBe(card);
    }
  });

  it("calls preventDefault on the Space key handler", () => {
    // jsdom cannot observe scrolling, so assert on defaultPrevented directly rather than on any
    // scroll side effect.
    const onSelect = vi.fn();
    render(
      <InspectorOpenerCard label="Open" onSelect={onSelect} panelId="panel" selected={false}>
        Body
      </InspectorOpenerCard>,
    );
    const card = screen.getByRole("button", { name: "Open" });

    let observedEvent: KeyboardEvent | undefined;
    card.addEventListener("keydown", (event) => {
      observedEvent = event;
    });
    fireEvent.keyDown(card, { key: " " });

    expect(observedEvent?.defaultPrevented).toBe(true);
  });

  it("merges a caller-supplied className with the geometry instead of clobbering it", () => {
    render(
      <InspectorOpenerCard
        className="border-destructive"
        label="Open"
        onSelect={vi.fn()}
        panelId="panel"
        selected={false}
      >
        Body
      </InspectorOpenerCard>,
    );
    const card = screen.getByRole("button", { name: "Open" });

    expect(card).toHaveClass("border-destructive");
    expect(card).toHaveClass("cursor-pointer");
    expect(card).toHaveClass("rounded-lg");
  });
});
