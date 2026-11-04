import { render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { ResourceCard } from "@/components/ui/resource-card";
import type { StatusProjection } from "@/lib/status-projection";

function renderCard(status?: StatusProjection, description?: string) {
  return render(
    <ResourceCard
      accessibleName="Gemma"
      colorKey="model-1"
      description={description ?? "Gemma 4 E2B · fast-start"}
      name="Gemma"
      onSelect={vi.fn()}
      panelId="panel"
      selected={false}
      status={status}
    />,
  );
}

/** The elements that occupy a line box, in the order the slot rule fixes them. */
function slotText(card: HTMLElement): string[] {
  return [...card.querySelectorAll('[data-slot="badge"], h3, [data-slot="card-description"]')].map(
    (element) => element.textContent ?? "",
  );
}

describe("ResourceCard", () => {
  it("renders three slots: the heading and badge on one row, the description beneath", () => {
    renderCard({ attention: "40%", label: "Loading", tone: "info" });

    const card = screen.getByRole("article", { name: "Gemma" });
    expect(slotText(card)).toEqual(["Gemma", "Loading", "Gemma 4 E2B · fast-start"]);

    const badge = within(card).getByText("Loading");
    expect(badge.parentElement).toContainElement(within(card).getByRole("heading", { level: 3 }));
  });

  it("seats the heading identically whether or not the card has a status", () => {
    const rowOf = (status?: StatusProjection) => {
      const { unmount } = renderCard(status);
      const heading = screen.getByRole("heading", { level: 3 });
      const row = heading.parentElement?.parentElement?.className ?? "";
      unmount();
      return row;
    };

    const badged = rowOf({ label: "Ready", tone: "success" });
    expect(badged).toBe(rowOf(undefined));
    // The badge's line box is taller than the heading's, so the row is pinned to a height both fit
    // in; without it a badged card seats its heading a fraction lower than a badgeless neighbour.
    expect(badged).toContain("min-h-6");
  });

  it("omits the slots it cannot fill instead of reserving them empty", () => {
    render(
      <ResourceCard
        accessibleName="Gemma"
        colorKey="model-1"
        name="Gemma"
        onSelect={vi.fn()}
        panelId="panel"
        selected={false}
      />,
    );

    const card = screen.getByRole("article", { name: "Gemma" });
    expect(card.querySelectorAll('[data-slot="badge"]')).toHaveLength(0);
    expect(card.querySelectorAll('[data-slot="card-description"]')).toHaveLength(0);
    expect(within(card).getByRole("heading", { level: 3 })).toHaveTextContent("Gemma");
  });

  it("renders exactly one badge whatever the status says", () => {
    renderCard({ attention: "Engine exited", label: "Failed", tone: "destructive" });

    expect(
      screen.getByRole("article", { name: "Gemma" }).querySelectorAll('[data-slot="badge"]'),
    ).toHaveLength(1);
  });

  it("renders the same line boxes in every state, so height cannot change with status", () => {
    const states: StatusProjection[] = [
      { label: "Ready", tone: "success" },
      { attention: "40%", label: "Loading", tone: "info" },
      { attention: "Engine exited with status 1", label: "Failed", tone: "destructive" },
    ];

    const counts = states.map((status) => {
      const { unmount } = renderCard(status);
      const count = slotText(screen.getByRole("article", { name: "Gemma" })).length;
      unmount();
      return count;
    });

    expect(counts).toEqual([3, 3, 3]);
  });

  it("renders a failed card identically to a healthy one but for its badge", () => {
    const markupFor = (status: StatusProjection) => {
      const { unmount } = renderCard(status);
      const card = screen.getByRole("article", { name: "Gemma" });
      card.querySelector('[data-slot="badge"]')?.remove();
      const markup = card.innerHTML;
      unmount();
      return markup;
    };

    expect(
      markupFor({ attention: "Engine exited with status 1", label: "Failed", tone: "destructive" }),
    ).toBe(markupFor({ label: "Ready", tone: "success" }));
  });

  it("never renders the attention label as text, carrying it on the badge's tooltip instead", () => {
    const attention = "Container exited during startup: port 8080 already bound by another service";
    renderCard({ attention, label: "Failed", tone: "destructive" });

    const card = screen.getByRole("article", { name: "Gemma" });
    expect(within(card).queryByText(attention)).not.toBeInTheDocument();
    expect(card.querySelector('[data-slot="badge"]')).toHaveAttribute("title", attention);
  });

  it("truncates an over-long name so the badge keeps the row's right edge", () => {
    render(
      <ResourceCard
        accessibleName="Long"
        colorKey="model-1"
        description="Llama 3.1 8B Instruct · a100-bf16"
        name="llama-3-1-8b-instruct-a100-bf16-west"
        onSelect={vi.fn()}
        panelId="panel"
        selected={false}
        status={{ label: "Failed", tone: "destructive" }}
      />,
    );

    // jsdom computes no layout, so the truncation contract is asserted on the rules that produce
    // it; the rendered result is verified in the browser.
    const heading = screen.getByRole("heading", { level: 3 });
    expect(heading).toHaveClass("truncate");
    expect(heading).toHaveAttribute("title", "llama-3-1-8b-instruct-a100-bf16-west");
    expect(heading.parentElement).toHaveClass("min-w-0");
  });

  it("fills its grid row so cards in one row share a height", () => {
    renderCard({ label: "Ready", tone: "success" });

    const card = screen.getByRole("article", { name: "Gemma" });
    expect(card).toHaveClass("h-full");
    expect(screen.getByRole("button", { name: "View details for Gemma" })).toHaveClass("h-full");
  });
});
