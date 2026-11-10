import { act, render, screen, waitFor, within } from "@testing-library/react";
import { createRef, StrictMode, useState } from "react";
import { createMemoryRouter, MemoryRouter, RouterProvider, useLocation } from "react-router-dom";
import { describe, expect, expectTypeOf, it, vi } from "vitest";

import { PanelNavigationProvider } from "@/components/ui/panel-navigation-provider";
import type { DeploymentMetadata, DeploymentMetadataUpdate } from "@/domain/deployment-metadata";
import { waitForInspectorFocus } from "@/test/inspector-actions";
import { expectResourceMarker, inspectorTitle } from "@/test/resource-marker";
import { setupUser } from "@/test/user";

import { DeploymentEditor, type DeploymentEditorProps } from "./deployment-editor";

expectTypeOf<DeploymentMetadata>().toEqualTypeOf<Readonly<{ displayName: string }>>();
expectTypeOf<DeploymentMetadataUpdate>().toEqualTypeOf<{
  metadata: DeploymentMetadata;
  fields: readonly (keyof DeploymentMetadata)[];
}>();
expectTypeOf<DeploymentEditorProps>().toMatchTypeOf<{
  metadata: DeploymentMetadata;
  onSaveMetadata: (update: DeploymentMetadataUpdate) => Promise<DeploymentMetadata>;
}>();

interface DeferredSave {
  promise: Promise<DeploymentMetadata>;
  resolve: (value: DeploymentMetadata) => void;
}

function deferredSave(): DeferredSave {
  let resolve!: (value: DeploymentMetadata) => void;
  const promise = new Promise<DeploymentMetadata>((nextResolve) => {
    resolve = nextResolve;
  });
  return { promise, resolve };
}

function LocationProbe() {
  const location = useLocation();
  return <output data-testid="location">{location.pathname + location.search}</output>;
}

function TerminalSaveHarness({ save }: { save: DeferredSave }) {
  const [terminal, setTerminal] = useState(false);
  return (
    <>
      <button onClick={() => setTerminal(true)} type="button">
        Observe teardown
      </button>
      <LocationProbe />
      <DeploymentEditor
        colorKey="deployment-1"
        name="Initial name"
        deploymentId="deployment-1"
        metadata={{ displayName: "Initial name" }}
        onClose={() => undefined}
        onSaveMetadata={() => save.promise}
        open
        openerRef={createRef()}
        terminal={terminal}
      />
    </>
  );
}

describe("DeploymentEditor layout", () => {
  it("separates deployment identity, the Name field, and its action", () => {
    const router = createMemoryRouter(
      [
        {
          path: "*",
          element: (
            <PanelNavigationProvider>
              <DeploymentEditor
                colorKey="deployment-1"
                name="Initial name"
                deploymentId="deployment-1"
                metadata={{ displayName: "Initial name" }}
                onClose={() => undefined}
                onSaveMetadata={() => Promise.resolve({ displayName: "Initial name" })}
                open
                openerRef={createRef()}
              />
            </PanelNavigationProvider>
          ),
        },
      ],
      { initialEntries: ["/models?panel=models%2Fedit%3Adeployment-1"] },
    );
    render(<RouterProvider router={router} />);

    const editor = screen.getByRole("complementary", { name: "Initial name · Edit" });
    const name = within(editor).getByRole("textbox", { name: "Name" });
    const section = name.closest("section");
    expectResourceMarker(inspectorTitle(editor), "deployment-1");

    expect(section).toHaveClass("mt-6");
    expect(name.parentElement).toHaveClass("grid", "gap-1");
    expect(within(section!).getByRole("button", { name: "Save" })).toHaveClass("mt-4");
    expect(within(editor).queryByRole("button", { name: "Save name" })).not.toBeInTheDocument();
  });
});

describe("DeploymentEditor metadata saves", () => {
  it("submits and receives accepted metadata rather than a Name scalar", async () => {
    const user = setupUser();
    const onSave = vi.fn().mockResolvedValue({ displayName: "Accepted name" });
    const router = createMemoryRouter(
      [
        {
          path: "*",
          element: (
            <PanelNavigationProvider>
              <DeploymentEditor
                colorKey="deployment-1"
                name="Initial name"
                deploymentId="deployment-1"
                metadata={{ displayName: "Initial name" }}
                onClose={() => undefined}
                onSaveMetadata={onSave}
                open
                openerRef={createRef()}
              />
            </PanelNavigationProvider>
          ),
        },
      ],
      { initialEntries: ["/models?panel=models%2Fedit%3Adeployment-1"] },
    );
    render(<RouterProvider router={router} />);

    const editor = screen.getByRole("complementary", { name: "Initial name · Edit" });
    await waitForInspectorFocus(editor);
    const name = within(editor).getByRole("textbox", { name: "Name" });
    await user.clear(name);
    await user.type(name, "Accepted name");
    await user.click(within(editor).getByRole("button", { name: "Save" }));

    expect(onSave).toHaveBeenCalledWith({
      fields: ["displayName"],
      metadata: { displayName: "Accepted name" },
    });
  });
});

describe("DeploymentEditor metadata synchronization", () => {
  it("synchronizes clean metadata, retains dirty drafts, and emits no render-phase warning", async () => {
    const user = setupUser();
    const consoleError = vi.spyOn(console, "error");
    const onSaveMetadata = vi.fn().mockResolvedValue({ displayName: "Initial name" });
    const renderEditor = (metadata: DeploymentMetadata) => (
      <StrictMode>
        <MemoryRouter>
          <DeploymentEditor
            colorKey="deployment-1"
            name="Initial name"
            deploymentId="deployment-1"
            metadata={metadata}
            onClose={() => undefined}
            onSaveMetadata={onSaveMetadata}
            open
            openerRef={createRef()}
          />
        </MemoryRouter>
      </StrictMode>
    );

    try {
      const { rerender } = render(renderEditor({ displayName: "Initial name" }));
      const editor = screen.getByRole("complementary", { name: "Initial name · Edit" });
      const name = within(editor).getByRole("textbox", { name: "Name" });

      rerender(renderEditor({ displayName: "Projected name" }));
      await waitFor(() => expect(name).toHaveValue("Projected name"));

      await user.clear(name);
      await user.type(name, "Unsaved name");
      rerender(renderEditor({ displayName: "Authoritative name" }));
      await waitFor(() => expect(name).toHaveValue("Unsaved name"));

      expect(
        consoleError.mock.calls.some((args) =>
          args
            .map((argument) => String(argument))
            .join(" ")
            .includes("Cannot update a component"),
        ),
      ).toBe(false);
    } finally {
      consoleError.mockRestore();
    }
  });
});

describe("DeploymentEditor terminal saves", () => {
  it.each(["local model", "application"])(
    "does not return after a terminal transition settles a pending final-clean %s save",
    async () => {
      const user = setupUser();
      const save = deferredSave();
      const router = createMemoryRouter(
        [
          {
            path: "*",
            element: (
              <PanelNavigationProvider>
                <TerminalSaveHarness save={save} />
              </PanelNavigationProvider>
            ),
          },
        ],
        {
          initialEntries: [
            "/models?panel=models%2Fdeployment%3Adeployment-1&panel=models%2Fedit%3Adeployment-1",
          ],
        },
      );
      render(<RouterProvider router={router} />);

      const editor = screen.getByRole("complementary", { name: "Initial name · Edit" });
      await waitFor(() => expect(inspectorTitle(editor)).toHaveFocus());
      const name = within(editor).getByLabelText("Name", { selector: "input" });
      await user.clear(name);
      await user.type(name, "Accepted name");
      await user.click(within(editor).getByRole("button", { name: "Save" }));

      expect(screen.getByRole("button", { name: "Saving…" })).toBeDisabled();
      expect(screen.getByRole("button", { name: "Back to model deployment" })).toBeDisabled();
      await user.click(screen.getByRole("button", { name: "Observe teardown" }));
      expect(
        screen.getByText("This deployment is tearing down and can no longer be edited."),
      ).toBeVisible();

      await act(async () => {
        save.resolve({ displayName: "Accepted name" });
        await Promise.resolve();
      });

      await waitFor(() =>
        expect(screen.getByRole("button", { name: "Back to model deployment" })).toBeEnabled(),
      );
      expect(screen.getByTestId("location")).toHaveTextContent(
        "/models?panel=models%2Fdeployment%3Adeployment-1&panel=models%2Fedit%3Adeployment-1",
      );
      expect(within(editor).getByRole("button", { name: "Save" })).toBeDisabled();
      expect(screen.getByRole("button", { name: "Close inspector" })).toBeEnabled();

      await user.click(screen.getByRole("button", { name: "Back to model deployment" }));
      await waitFor(() =>
        expect(screen.getByTestId("location")).toHaveTextContent(
          "/models?panel=models%2Fdeployment%3Adeployment-1",
        ),
      );
    },
  );
});
