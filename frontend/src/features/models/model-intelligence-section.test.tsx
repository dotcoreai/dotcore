import { render, screen, within } from "@testing-library/react";
import { createRef } from "react";
import { MemoryRouter } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";

import type { Model } from "@/gen/dotcore/catalog/v1/model_pb";

import { projectModelRows } from "./catalog-model-row";
import { syntheticCatalog } from "./catalog-table-test-support";
import { CatalogModelInspector } from "./model-inspector";

const { engineReleases, hardwareClasses, models } = syntheticCatalog();
const [alpha, bare] = models;

function renderInspector(model: Model) {
  const [row] = projectModelRows({
    deployments: [],
    engineReleases,
    hardwareClasses,
    estimates: new Map(),
    models: [model],
  });
  render(
    <MemoryRouter>
      <CatalogModelInspector
        memoryPending={false}
        model={model}
        onDeploy={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
        row={row}
      />
    </MemoryRouter>,
  );
}

describe("ModelIntelligenceSection", () => {
  it("shows only the overall score as a bar", () => {
    renderInspector(alpha);

    const section = screen.getByRole("region", { name: "Intelligence" });
    const overall = within(section).getByRole("progressbar", { name: "Overall" });
    expect(overall).toHaveAttribute("aria-valuenow", "41.5");
    expect(overall).toHaveAttribute("aria-valuetext", "41.5 out of 100");
    expect(overall).toHaveTextContent("41.5");
    expect(within(section).getAllByRole("progressbar")).toEqual([overall]);
    expect(within(section).getByRole("heading").parentElement).toHaveTextContent(/^Intelligence$/);
    expect(within(section).getByText("Dotcore Bench Intelligence Score")).toBeVisible();
  });

  it("renders nothing for a model without a score", () => {
    renderInspector(bare);

    expect(screen.getByRole("region", { name: "Catalog model" })).toBeVisible();
    expect(screen.queryByRole("region", { name: "Intelligence" })).not.toBeInTheDocument();
    expect(screen.queryByRole("progressbar", { name: "Overall" })).not.toBeInTheDocument();
  });
});
