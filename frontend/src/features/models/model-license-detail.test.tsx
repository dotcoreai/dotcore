import { clone, create } from "@bufbuild/protobuf";
import { render, screen, within } from "@testing-library/react";
import { createRef } from "react";
import { MemoryRouter } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";

import { LicenseSchema, type Model, ModelSchema } from "@/gen/dotcore/catalog/v1/model_pb";

import { projectModelRows } from "./catalog-model-row";
import { syntheticCatalog } from "./catalog-table-test-support";
import { CatalogModelInspector } from "./model-inspector";

const { engineReleases, hardwareClasses, models } = syntheticCatalog();
const [, bare] = models;

function withLicense(id: string, termsUrl: string): Model {
  const model = clone(ModelSchema, bare);
  model.license = create(LicenseSchema, { id, termsUrl });
  return model;
}

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

describe("ModelLicenseDetail", () => {
  it("links an SPDX identifier to its terms", () => {
    renderInspector(withLicense("Apache-2.0", "https://www.apache.org/licenses/LICENSE-2.0"));

    const section = screen.getByRole("region", { name: "Catalog model" });
    expect(within(section).getByText("License")).toBeVisible();
    const link = within(section).getByRole("link", { name: "Apache-2.0" });
    expect(link).toHaveAttribute("href", "https://www.apache.org/licenses/LICENSE-2.0");
    expect(link).toHaveAttribute("target", "_blank");
    expect(link).toHaveAttribute("rel", "noopener noreferrer");
  });

  it("links a custom licence to its terms", () => {
    renderInspector(withLicense("custom", "https://example.test/terms"));

    const link = within(screen.getByRole("region", { name: "Catalog model" })).getByRole("link", {
      name: "custom",
    });
    expect(link).toHaveAttribute("href", "https://example.test/terms");
  });

  it("shows an identifier without a link when the declaration has no terms URL", () => {
    renderInspector(withLicense("MIT", ""));

    const section = screen.getByRole("region", { name: "Catalog model" });
    expect(within(section).getByText("MIT")).toBeVisible();
    expect(within(section).queryByRole("link")).not.toBeInTheDocument();
  });

  it("renders nothing for a model without a licence", () => {
    renderInspector(bare);

    const section = screen.getByRole("region", { name: "Catalog model" });
    expect(within(section).queryByText("License")).not.toBeInTheDocument();
  });
});
