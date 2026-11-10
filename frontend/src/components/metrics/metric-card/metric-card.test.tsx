import { render, screen } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it } from "vitest";

import { setupUser } from "@/test/user";

import { Sparkline } from "../sparkline/sparkline";
import { MetricCard, MetricCardDetail, MetricCardGrid } from "./metric-card";

function Grid({ pending = false }: { pending?: boolean }) {
  const [selected, setSelected] = useState<string | undefined>();

  return (
    <MetricCardGrid onSelectedChange={setSelected} selected={selected}>
      <MetricCard
        id="requests"
        label="Requests"
        line="3 failed"
        pending={pending}
        sparkline={<Sparkline values={[1, 2]} />}
        unit="req"
        value="42"
      />
      <MetricCard id="ttft" label="TTFT" pending={pending} unit="ms" value="310" />
    </MetricCardGrid>
  );
}

describe("MetricCardGrid", () => {
  it("shows label, value, unit and line, and selects one card at a time", async () => {
    const user = setupUser();
    render(<Grid />);

    const requests = screen.getByRole("button", { name: /Requests/ });
    expect(requests).toHaveTextContent("Requests42 req3 failed");
    expect(requests).toHaveAttribute("aria-pressed", "false");

    await user.click(requests);
    expect(requests).toHaveAttribute("aria-pressed", "true");
    expect(requests).toHaveAttribute("data-selected", "true");

    const ttft = screen.getByRole("button", { name: /TTFT/ });
    await user.click(ttft);
    expect(ttft).toHaveAttribute("aria-pressed", "true");
    expect(requests).toHaveAttribute("aria-pressed", "false");
    expect(requests).not.toHaveAttribute("data-selected", "true");
  });

  it("clears the selection when the selected card is chosen again", async () => {
    const user = setupUser();
    render(<Grid />);

    const requests = screen.getByRole("button", { name: /Requests/ });
    await user.click(requests);
    await user.click(requests);

    expect(requests).toHaveAttribute("aria-pressed", "false");
    expect(document.querySelector('[data-selected="true"]')).toBeNull();
  });

  it("toggles from the keyboard with Enter and Space", async () => {
    const user = setupUser();
    render(<Grid />);

    await user.tab();
    const requests = screen.getByRole("button", { name: /Requests/ });
    expect(requests).toHaveFocus();

    await user.keyboard("{Enter}");
    expect(requests).toHaveAttribute("aria-pressed", "true");
    await user.keyboard(" ");
    expect(requests).toHaveAttribute("aria-pressed", "false");
  });

  it("keeps a pending card's slots, with a dash for the value and no unit", () => {
    const ready = render(<Grid />);
    const readySlots = cardSlots("Requests");
    ready.unmount();

    render(<Grid pending />);

    expect(cardSlots("Requests")).toEqual(readySlots);
    const card = screen.getByRole("button", { name: /Requests/ });
    expect(card.querySelector('[data-slot="metric-card-value"]')).toHaveTextContent(/^—$/);
    expect(card.querySelector('[data-slot="metric-card-line"]')).toBeInTheDocument();
    expect(card.querySelector('[data-slot="sparkline"]')).toBeInTheDocument();
  });
});

describe("MetricCard", () => {
  it("keeps the line slot with or without a line, so cards share one height", () => {
    render(
      <MetricCardGrid onSelectedChange={() => {}} selected={undefined}>
        <MetricCard id="with" label="Requests" line="3 failed" value="1" />
        <MetricCard id="without" label="Tokens" value="2" />
        <MetricCard id="empty" label="Speed" line={undefined} value="3" />
      </MetricCardGrid>,
    );

    expect(["Requests", "Tokens", "Speed"].map((name) => cardSlots(name))).toEqual(
      Array.from({ length: 3 }, () => [
        "metric-card-label",
        "metric-card-value",
        "metric-card-line",
      ]),
    );
    expect(
      screen
        .getByRole("button", { name: /Tokens/ })
        .querySelector('[data-slot="metric-card-line"]'),
    ).toHaveTextContent(/^$/);
  });
});

describe("MetricCardDetail", () => {
  it("shows the title and chart, and switches the split", async () => {
    const user = setupUser();
    const splits: string[] = [];
    render(
      <MetricCardDetail
        onSplitChange={(next) => splits.push(next)}
        split="model"
        splitOptions={[
          { label: "Model", value: "model" },
          { label: "Route", value: "route" },
        ]}
        title="Requests"
      >
        <div>chart</div>
      </MetricCardDetail>,
    );

    expect(screen.getByRole("heading", { name: "Requests" })).toBeVisible();
    expect(screen.getByText("chart")).toBeVisible();

    await user.click(screen.getByRole("radio", { name: "Route" }));
    expect(splits).toEqual(["route"]);
  });

  it("omits the split switch without two options", () => {
    render(
      <MetricCardDetail title="Requests">
        <div>chart</div>
      </MetricCardDetail>,
    );

    expect(screen.queryByRole("radiogroup")).not.toBeInTheDocument();
  });
});

function cardSlots(name: string) {
  const card = screen.getByRole("button", { name: new RegExp(name) });

  return Array.from(card.querySelectorAll("[data-slot]"), (el) => el.getAttribute("data-slot"));
}
