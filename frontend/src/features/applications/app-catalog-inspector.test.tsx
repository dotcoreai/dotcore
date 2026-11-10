import { create } from "@bufbuild/protobuf";
import { render as renderUi, screen, within } from "@testing-library/react";
import { createRef } from "react";
import { describe, expect, it, vi } from "vitest";

import {
  ApplicationGraphSchema,
  ApplicationSchema,
  ApplicationServiceSchema,
} from "@/gen/dotcore/catalog/v1/application_pb";
import { expectResourceMarker, inspectorTitle } from "@/test/resource-marker";
import { setupUser } from "@/test/user";

import { CatalogApplicationInspector } from "./app-catalog-inspector";

const application = create(ApplicationSchema, {
  applicationId: "open-webui",
  displayName: "OpenWebUI",
  version: "0.6.0",
});

describe("Catalog application inspector", () => {
  it("keeps catalog details separate and opens the exact deployment dialog", async () => {
    const user = setupUser();
    const onDeploy = vi.fn();
    renderUi(
      <CatalogApplicationInspector
        application={application}
        onDeploy={onDeploy}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );
    expect(screen.getByRole("region", { name: "Catalog application" })).toBeVisible();
    expect(screen.queryByText("Services")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("Execution target")).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    expect(onDeploy).toHaveBeenCalledOnce();
  });

  it("shows notes in their own section only when the application declares them", () => {
    const renderInspector = (notes: string) =>
      renderUi(
        <CatalogApplicationInspector
          application={create(ApplicationSchema, { ...application, notes })}
          onDeploy={vi.fn()}
          onOpenChange={vi.fn()}
          open
          openerRef={createRef<HTMLElement>()}
          pending={false}
        />,
      );

    const withNotes = renderInspector("First line.\nSecond line.");
    const notes = screen.getByRole("region", { name: "Notes" });
    expect(notes.querySelector("p")?.textContent).toBe("First line.\nSecond line.");
    expect(notes.querySelector("p")).toHaveClass("whitespace-pre-wrap");
    withNotes.unmount();

    renderInspector("");
    expect(screen.queryByRole("region", { name: "Notes" })).not.toBeInTheDocument();
  });

  it.each([
    { expected: "3 services", services: ["web", "worker", "cache"] },
    { expected: "1 service", services: ["web"] },
  ])("states the service count as the heading meta: $expected", ({ expected, services }) => {
    renderUi(
      <CatalogApplicationInspector
        application={create(ApplicationSchema, {
          ...application,
          graph: create(ApplicationGraphSchema, {
            services: services.map((serviceName) =>
              create(ApplicationServiceSchema, { serviceName }),
            ),
          }),
        })}
        onDeploy={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    const section = screen.getByRole("region", { name: "Catalog application" });
    expect(within(section).getByText(expected)).toBeVisible();
    expect(within(section).queryByText("Services")).not.toBeInTheDocument();
  });

  it("marks the catalog inspector title with the catalog application color", () => {
    renderUi(
      <CatalogApplicationInspector
        application={application}
        onDeploy={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );
    expectResourceMarker(
      inspectorTitle(screen.getByRole("complementary", { name: "OpenWebUI" })),
      "open-webui",
    );
  });
});
