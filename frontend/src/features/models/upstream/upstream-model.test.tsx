import { create } from "@bufbuild/protobuf";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { type ComponentProps, createRef, StrictMode, useState } from "react";
import { createMemoryRouter, MemoryRouter, RouterProvider, useLocation } from "react-router-dom";
import { describe, expect, expectTypeOf, it, vi } from "vitest";

import { PanelNavigationProvider } from "@/components/ui/panel-navigation-provider";
import type { DeploymentMetadata, DeploymentMetadataUpdate } from "@/domain/deployment-metadata";
import { ChatOpenerContext } from "@/features/chat/chat-opener";
import {
  counterPoint,
  distributionPoint,
  idleMetricsClient,
  metricsClientWith,
  metricSeries,
  queryResult,
  staticWatch,
} from "@/features/metrics/metric-test-support";
import {
  UpstreamModelDataEgressClass,
  UpstreamModelLocality,
  UpstreamModelTemplateSchema,
} from "@/gen/dotcore/catalog/v1/upstream_model_template_pb";
import {
  ModelDeploymentDesiredState,
  ModelDeploymentSchema,
  ModelDeploymentSnapshotSchema,
  OpenAiCompatibleEndpointConfigurationSchema,
  UpstreamModelDeploymentSnapshotSchema,
  UpstreamModelDesiredConfigurationSchema,
  UpstreamModelInputCapabilitiesSchema,
} from "@/gen/dotcore/model/v1/deployment_pb";
import { ModelDeploymentStateSchema, ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import { usePanelNavigation } from "@/panels/panel-navigation";
import { PanelParentContext } from "@/panels/panel-titles-context";
import {
  chooseMoreAction,
  inspectorActionList,
  waitForInspectorFocus,
} from "@/test/inspector-actions";
import { expectResourceMarker, inspectorTitle } from "@/test/resource-marker";
import { setupUser } from "@/test/user";

import { UpstreamDeployDialog } from "./upstream-deploy-dialog";
import { UpstreamEditInspector } from "./upstream-edit-inspector";
import {
  defaultUpstreamForm,
  toUpstreamConfiguration,
  type UpstreamFormValues,
  validateModelId,
} from "./upstream-form";
import { UpstreamInspector } from "./upstream-inspector";

expectTypeOf<ComponentProps<typeof UpstreamEditInspector>>().toMatchTypeOf<{
  metadata: DeploymentMetadata;
  onSaveMetadata: (update: DeploymentMetadataUpdate) => Promise<DeploymentMetadata>;
}>();

const template = create(UpstreamModelTemplateSchema, {
  upstreamModelTemplateId: "generic-openai-compatible",
  displayName: "OpenAI-compatible",
});

function values(overrides: Partial<UpstreamFormValues> = {}): UpstreamFormValues {
  return {
    exposure: "direct",
    routeName: "public-route",
    upstreamModelId: "private-model",
    baseUrl: "https://endpoint.example/v1",
    apiKey: "secret-key",
    supportsText: true,
    supportsImage: true,
    locality: UpstreamModelLocality.PRIVATE,
    trustDomain: "endpoint.example",
    dataEgressClass: UpstreamModelDataEgressClass.PRIVATE_NETWORK,
    allowInsecureHttp: false,
    ...overrides,
  };
}

function hasRenderPhaseUpdateWarning(calls: unknown[][]): boolean {
  return calls.some((call) => {
    const message = call.map((argument) => String(argument)).join(" ");
    return (
      message.includes("Cannot update a component") &&
      message.includes("while rendering a different component") &&
      message.includes("UpstreamDeployDialog") &&
      message.includes("UpstreamForm")
    );
  });
}

function GuardedUpstreamEdit() {
  const navigation = usePanelNavigation();
  return (
    <UpstreamEditInspector
      metadata={{ displayName: "Upstream model" }}
      onSaveMetadata={() => Promise.resolve({ displayName: "Upstream model" })}
      deployment={upstreamDeployment()}
      onBack={navigation.back}
      onOpenChange={(open) => {
        if (!open) navigation.close();
      }}
      onSubmit={vi.fn()}
      open
      openerRef={createRef()}
    />
  );
}

interface DeferredSave {
  promise: Promise<{ displayName: string }>;
  resolve: (value: { displayName: string }) => void;
}

function deferredSave(): DeferredSave {
  let resolve!: (value: { displayName: string }) => void;
  const promise = new Promise<{ displayName: string }>((nextResolve) => {
    resolve = nextResolve;
  });
  return { promise, resolve };
}

function LocationProbe() {
  const location = useLocation();
  return <output data-testid="location">{location.pathname + location.search}</output>;
}

function TerminalUpstreamEdit({ save }: { save: DeferredSave }) {
  const navigation = usePanelNavigation();
  const [terminal, setTerminal] = useState(false);
  const [presentDeployment] = useState(upstreamDeployment);
  const deployment = terminal
    ? create(ModelDeploymentSchema, {
        ...presentDeployment,
        desiredState: ModelDeploymentDesiredState.TEARDOWN,
      })
    : presentDeployment;

  return (
    <>
      <button onClick={() => setTerminal(true)} type="button">
        Observe teardown
      </button>
      <LocationProbe />
      <UpstreamEditInspector
        metadata={{ displayName: "Upstream model" }}
        deployment={deployment}
        onBack={navigation.back}
        onOpenChange={(open) => {
          if (!open) navigation.close();
        }}
        onSaveEndpoint={() => save.promise.then(() => true)}
        onSaveMetadata={() => save.promise}
        open
        openerRef={createRef()}
      />
    </>
  );
}

function renderTerminalUpstreamEdit(save: DeferredSave) {
  const router = createMemoryRouter(
    [
      {
        path: "*",
        element: (
          <PanelNavigationProvider>
            <TerminalUpstreamEdit save={save} />
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
}

// Moves from Connection to Deploy.
async function openDeploy(dialog: HTMLElement, user: UserEvent) {
  await user.click(within(dialog).getByRole("button", { name: "Next" }));
  expect(within(dialog).getByText("Step 2 of 2 — Deploy")).toBeVisible();
}

/** Label and value of each Deploy summary row, in display order. */
function summaryRows(dialog: HTMLElement) {
  const list = within(dialog).getByRole("region", { name: "Summary" }).querySelector("dl")!;
  const labels = within(list).getAllByRole("term");
  const values = within(list).getAllByRole("definition");
  return labels.map((label, index) => [label.textContent, values[index]?.textContent]);
}

function upstreamDeployment() {
  return create(ModelDeploymentSchema, {
    modelDeploymentId: "deployment-1",
    desiredState: ModelDeploymentDesiredState.PRESENT,
    snapshot: create(ModelDeploymentSnapshotSchema, {
      source: {
        case: "upstream",
        value: create(UpstreamModelDeploymentSnapshotSchema, {
          upstreamModelTemplate: template,
        }),
      },
    }),
    state: create(ModelDeploymentStateSchema, { status: ModelDeploymentStatus.READY }),
    upstreamDesiredConfiguration: create(UpstreamModelDesiredConfigurationSchema, {
      upstreamModelId: "private-model",
      apiKey: "stored-secret",
      inputCapabilities: create(UpstreamModelInputCapabilitiesSchema, {
        supportsText: true,
        supportsImage: false,
      }),
      endpoint: create(OpenAiCompatibleEndpointConfigurationSchema, {
        baseUrl: "https://endpoint.example/v1",
      }),
      boundary: {
        locality: UpstreamModelLocality.PRIVATE,
        trustDomain: "endpoint.example",
        dataEgressClass: UpstreamModelDataEgressClass.PRIVATE_NETWORK,
      },
    }),
  });
}

describe("generic upstream endpoint UI", () => {
  it("titles the Deploy dialog with the upstream template marker", () => {
    render(
      <UpstreamDeployDialog onOpenChange={vi.fn()} onSubmit={vi.fn()} open template={template} />,
    );

    expectResourceMarker(
      inspectorTitle(screen.getByRole("dialog", { name: "Upstream model — Deploy model" })),
      template.upstreamModelTemplateId,
    );
  });

  it("validates nonblank upstream IDs", () => {
    expect(validateModelId("gpt-5.6-sol")).toBeUndefined();
    expect(validateModelId("")).toBe("Upstream model ID is required.");
    expect(validateModelId("   ")).toBe("Upstream model ID is required.");
  });

  it("accepts dotted upstream IDs and preserves them", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    render(
      <UpstreamDeployDialog open onOpenChange={vi.fn()} onSubmit={onSubmit} template={template} />,
    );
    const dialog = screen.getByRole("dialog", { name: "Upstream model — Deploy model" });
    await user.type(within(dialog).getByLabelText("Base URL"), "https://endpoint.example/v1");
    await user.type(within(dialog).getByLabelText("Upstream model ID"), " gpt-5.6-sol ");
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    await user.click(within(dialog).getByRole("button", { name: "Deploy" }));
    expect(onSubmit).toHaveBeenCalledWith(
      expect.objectContaining({
        name: "gpt-5.6-sol",
        routeName: "gpt-5.6-sol",
        upstreamModelId: " gpt-5.6-sol ",
      }),
    );
  });

  it("advances on Enter in Connection and submits once on Enter in Deploy", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    render(
      <UpstreamDeployDialog open onOpenChange={vi.fn()} onSubmit={onSubmit} template={template} />,
    );
    const dialog = screen.getByRole("dialog", { name: "Upstream model — Deploy model" });
    await user.type(within(dialog).getByLabelText("Base URL"), "https://endpoint.example/v1");
    await user.type(within(dialog).getByLabelText("Upstream model ID"), "private-model");
    await user.keyboard("{Enter}");

    expect(within(dialog).getByText("Step 2 of 2 — Deploy")).toBeVisible();
    expect(onSubmit).not.toHaveBeenCalled();
    await user.click(within(dialog).getByRole("textbox", { name: "Deployment name" }));
    await user.keyboard("{Enter}");
    expect(onSubmit).toHaveBeenCalledOnce();
  });

  it("keeps first-page actions available so required-field validation can explain errors", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    render(
      <UpstreamDeployDialog open onOpenChange={vi.fn()} onSubmit={onSubmit} template={template} />,
    );
    const dialog = screen.getByRole("dialog", { name: "Upstream model — Deploy model" });
    const baseUrl = within(dialog).getByLabelText("Base URL");

    expect(within(dialog).getByRole("checkbox", { name: "Advanced" })).toBeEnabled();
    expect(within(dialog).getByRole("button", { name: "Next" })).toBeEnabled();
    await user.click(within(dialog).getByRole("button", { name: "Next" }));

    expect(within(dialog).getByText("Step 1 of 2 — Connection")).toBeVisible();
    expect(within(dialog).getByText("Base URL is required.")).toBeVisible();
    expect(within(dialog).getByText("Upstream model ID is required.")).toBeVisible();
    expect(baseUrl).toHaveAttribute("aria-invalid", "true");
    expect(baseUrl).toHaveAccessibleDescription(
      "Address of the OpenAI-compatible server that runs the model. Base URL is required.",
    );
    expect(baseUrl).toHaveFocus();
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it("flags connection fields on blur only after they change", async () => {
    const user = setupUser();
    render(
      <UpstreamDeployDialog open onOpenChange={vi.fn()} onSubmit={vi.fn()} template={template} />,
    );
    const dialog = screen.getByRole("dialog", { name: "Upstream model — Deploy model" });
    const baseUrl = within(dialog).getByLabelText("Base URL");
    const modelId = within(dialog).getByLabelText("Upstream model ID");

    await user.click(baseUrl);
    await user.tab();
    await user.tab();
    expect(modelId).toHaveFocus();
    await user.tab();
    await user.click(within(dialog).getByRole("checkbox", { name: "Advanced" }));
    expect(baseUrl).not.toHaveAttribute("aria-invalid");
    expect(modelId).not.toHaveAttribute("aria-invalid");

    await user.type(baseUrl, "h");
    await user.clear(baseUrl);
    await user.tab();
    expect(await within(dialog).findByText("Base URL is required.")).toBeVisible();
    expect(baseUrl).toHaveAccessibleDescription(
      "Address of the OpenAI-compatible server that runs the model. Base URL is required.",
    );
    expect(modelId).not.toHaveAttribute("aria-invalid");
    expect(within(dialog).queryByText("Upstream model ID is required.")).not.toBeInTheDocument();

    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    expect(within(dialog).getByText("Step 1 of 2 — Connection")).toBeVisible();
    expect(modelId).toHaveAccessibleDescription(
      "The model name that server expects in requests. Upstream model ID is required.",
    );
    expect(baseUrl).toHaveFocus();
  });

  it("flags a cleared deployment name on blur and Deploy instead of a route notice", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    render(
      <UpstreamDeployDialog open onOpenChange={vi.fn()} onSubmit={onSubmit} template={template} />,
    );
    const dialog = screen.getByRole("dialog", { name: "Upstream model — Deploy model" });
    await user.type(within(dialog).getByLabelText("Base URL"), "https://endpoint.example/v1");
    await user.type(within(dialog).getByLabelText("Upstream model ID"), "private-model");
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    const name = within(dialog).getByRole("textbox", { name: "Deployment name" });

    await user.clear(name);
    expect(name).not.toHaveAttribute("aria-invalid");
    expect(within(dialog).queryByRole("alert")).not.toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeEnabled();

    await user.tab();
    expect(await within(dialog).findByText("Deployment name is required.")).toBeVisible();
    expect(name).toHaveAccessibleDescription("Deployment name is required.");

    await user.click(within(dialog).getByRole("button", { name: "Deploy" }));
    expect(name).toHaveFocus();
    expect(onSubmit).not.toHaveBeenCalled();

    await user.type(name, "Renamed");
    expect(name).not.toHaveAttribute("aria-invalid");
    await user.click(within(dialog).getByRole("button", { name: "Deploy" }));
    expect(onSubmit).toHaveBeenCalledWith(
      expect.objectContaining({ name: "Renamed", routeName: "renamed" }),
    );
  });

  it("keeps an untouched invalid deployment name quiet until Deploy", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    render(
      <UpstreamDeployDialog open onOpenChange={vi.fn()} onSubmit={onSubmit} template={template} />,
    );
    const dialog = screen.getByRole("dialog", { name: "Upstream model — Deploy model" });
    await user.type(within(dialog).getByLabelText("Base URL"), "https://endpoint.example/v1");
    // The derived deployment name keeps the control character the model ID allows.
    fireEvent.change(within(dialog).getByLabelText("Upstream model ID"), {
      target: { value: "model\u0007" },
    });
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    const name = within(dialog).getByRole("textbox", { name: "Deployment name" });

    await user.click(name);
    await user.tab();
    await user.click(within(dialog).getByRole("checkbox", { name: "Advanced" }));
    expect(name).not.toHaveAttribute("aria-invalid");

    await user.click(within(dialog).getByRole("button", { name: "Deploy" }));
    expect(name).toHaveAccessibleDescription("Deployment name cannot contain control characters.");
    expect(name).toHaveFocus();
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it("flags the route name on blur only after it changes", async () => {
    const user = setupUser();
    render(
      <UpstreamDeployDialog open onOpenChange={vi.fn()} onSubmit={vi.fn()} template={template} />,
    );
    const dialog = screen.getByRole("dialog", { name: "Upstream model — Deploy model" });
    await user.type(within(dialog).getByLabelText("Base URL"), "https://endpoint.example/v1");
    await user.type(within(dialog).getByLabelText("Upstream model ID"), "private-model");
    await openDeploy(dialog, user);
    const route = within(dialog).getByLabelText("Route name");

    await user.click(route);
    await user.tab();
    expect(route).not.toHaveAttribute("aria-invalid");

    await user.type(route, "Bad Route");
    await user.tab();
    await waitFor(() => expect(route).toHaveAttribute("aria-invalid", "true"));
    expect(route).toHaveAccessibleDescription(
      expect.stringContaining("Route name must start with a letter"),
    );
  });

  it("shows a derived route collision on the route field as Deploy opens until renamed", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    render(
      <UpstreamDeployDialog
        existingRouteNames={["private-model"]}
        open
        onOpenChange={vi.fn()}
        onSubmit={onSubmit}
        template={template}
      />,
    );
    const dialog = screen.getByRole("dialog", { name: "Upstream model — Deploy model" });
    await user.type(within(dialog).getByLabelText("Base URL"), "https://endpoint.example/v1");
    await user.type(within(dialog).getByLabelText("Upstream model ID"), "private-model");
    await openDeploy(dialog, user);

    const route = within(dialog).getByLabelText("Route name");
    expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeDisabled();
    expect(route).toHaveAttribute("aria-invalid", "true");
    expect(route).toHaveAccessibleDescription(
      'The model name callers send in requests. It follows the deployment name until you change it. An inference route named "private-model" already exists. Change the route or the deployment name.',
    );

    const name = within(dialog).getByRole("textbox", { name: "Deployment name" });
    await user.clear(name);
    await user.type(name, "Private model two");
    expect(within(dialog).queryByText(/already exists/)).not.toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Deploy" }));
    expect(onSubmit).toHaveBeenCalledWith(
      expect.objectContaining({ name: "Private model two", routeName: "private-model-two" }),
    );
  });

  it("clears a derived route collision when the route is edited", async () => {
    const user = setupUser();
    render(
      <UpstreamDeployDialog
        existingRouteNames={["private-model"]}
        open
        onOpenChange={vi.fn()}
        onSubmit={vi.fn()}
        template={template}
      />,
    );
    const dialog = screen.getByRole("dialog", { name: "Upstream model — Deploy model" });
    await user.type(within(dialog).getByLabelText("Base URL"), "https://endpoint.example/v1");
    await user.type(within(dialog).getByLabelText("Upstream model ID"), "private-model");
    await openDeploy(dialog, user);

    const route = within(dialog).getByLabelText("Route name");
    await user.clear(route);
    await user.type(route, "other-route");

    expect(within(dialog).queryByText(/already exists/)).not.toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeEnabled();
  });

  it("shows a backend route conflict on the route field until the route changes", async () => {
    const user = setupUser();
    render(
      <UpstreamDeployDialog
        open
        onOpenChange={vi.fn()}
        onSubmit={vi.fn()}
        routeConflict={{ routeName: "private-model" }}
        template={template}
      />,
    );
    const dialog = screen.getByRole("dialog", { name: "Upstream model — Deploy model" });
    await user.type(within(dialog).getByLabelText("Base URL"), "https://endpoint.example/v1");
    await user.type(within(dialog).getByLabelText("Upstream model ID"), "private-model");
    await openDeploy(dialog, user);

    const route = within(dialog).getByLabelText("Route name");
    expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeDisabled();
    expect(route).toHaveAccessibleDescription(
      'The model name callers send in requests. It follows the deployment name until you change it. An inference route named "private-model" already exists. Change the route or the deployment name.',
    );

    await user.clear(route);
    await user.type(route, "other-route");
    expect(within(dialog).queryByText(/already exists/)).not.toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeEnabled();
  });

  it("keeps edited values when Back returns to Connection and Advanced is toggled", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    render(
      <UpstreamDeployDialog open onOpenChange={vi.fn()} onSubmit={onSubmit} template={template} />,
    );
    const dialog = screen.getByRole("dialog", { name: "Upstream model — Deploy model" });
    await user.type(within(dialog).getByLabelText("Base URL"), "https://endpoint.example/v1");
    await user.type(within(dialog).getByLabelText("Upstream model ID"), "private-model");
    await user.click(within(dialog).getByRole("checkbox", { name: "Advanced" }));
    await openDeploy(dialog, user);

    const route = within(dialog).getByLabelText("Route name");
    await user.clear(route);
    await user.type(route, "custom-route");
    await user.click(within(dialog).getByRole("button", { name: "Back" }));

    expect(within(dialog).getByText("Step 1 of 2 — Connection")).toBeVisible();
    expect(within(dialog).getByRole("checkbox", { name: "Advanced" })).toBeChecked();
    expect(within(dialog).getByLabelText("Base URL")).toHaveValue("https://endpoint.example/v1");

    await user.click(within(dialog).getByRole("checkbox", { name: "Advanced" }));
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    expect(within(dialog).getByText("Step 2 of 2 — Deploy")).toBeVisible();
    expect(within(dialog).queryByRole("radio", { name: "Hidden" })).not.toBeInTheDocument();
    expect(within(dialog).getByLabelText("Route name")).toHaveValue("custom-route");
    const name = within(dialog).getByRole("textbox", { name: "Deployment name" });
    await user.clear(name);
    await user.type(name, "Customized name");
    await user.click(within(dialog).getByRole("button", { name: "Deploy" }));
    expect(onSubmit).toHaveBeenCalledWith(
      expect.objectContaining({ name: "Customized name", routeName: "custom-route" }),
    );
  });

  it("navigates visited steps from their headers and validates before moving forward", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    render(
      <UpstreamDeployDialog open onOpenChange={vi.fn()} onSubmit={onSubmit} template={template} />,
    );
    const dialog = screen.getByRole("dialog", { name: "Upstream model — Deploy model" });
    await user.type(within(dialog).getByLabelText("Base URL"), "https://endpoint.example/v1");
    await user.type(within(dialog).getByLabelText("Upstream model ID"), "private-model");

    const progress = within(dialog).getByRole("list", { name: "Deployment progress" });
    expect(
      Array.from(progress.querySelectorAll("[data-step-label]"), (label) => label.textContent),
    ).toEqual(["1. Connection", "2. Deploy"]);
    expect(within(progress).getByRole("button", { name: "2. Deploy" })).toBeDisabled();

    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    expect(within(progress).getByRole("button", { current: "step" })).toHaveTextContent(
      "2. Deploy",
    );
    expect(
      within(within(dialog).getByTestId("deployment-dialog-actions"))
        .getAllByRole("button")
        .map((button) => button.textContent),
    ).toEqual(["Cancel", "Back", "Deploy"]);

    await user.click(within(progress).getByRole("button", { name: "1. Connection" }));
    expect(within(dialog).getByText("Step 1 of 2 — Connection")).toBeVisible();
    await user.click(within(progress).getByRole("button", { name: "2. Deploy" }));

    const route = within(dialog).getByLabelText("Route name");
    expect(route).toHaveValue("private-model");
    await user.clear(route);
    await user.type(route, "Bad Route");
    await user.click(within(dialog).getByRole("button", { name: "Deploy" }));
    expect(onSubmit).not.toHaveBeenCalled();
    expect(route).toHaveAttribute("aria-invalid", "true");
    expect(route).toHaveAccessibleDescription(
      expect.stringContaining(
        "Route name must start with a letter and contain only lowercase letters, numbers, hyphens, and periods.",
      ),
    );
    expect(route).toHaveFocus();

    await user.clear(route);
    await user.type(route, "renamed");
    await user.click(within(dialog).getByRole("button", { name: "Deploy" }));
    expect(onSubmit).toHaveBeenCalledWith(
      expect.objectContaining({ name: "private-model", routeName: "renamed" }),
    );
  });

  it("offers Connect for the target route, or disabled with the reason there is none", async () => {
    const user = setupUser();
    const onConnect = vi.fn();
    const props = {
      deployment: upstreamDeployment(),
      metricsClient: idleMetricsClient(),
      onConnect,
      onEdit: vi.fn(),
      onOpenChange: vi.fn(),
      onUnload: vi.fn(),
      open: true,
      openerRef: createRef<HTMLButtonElement>(),
    };
    const { rerender } = render(
      <MemoryRouter>
        <UpstreamInspector {...props} connectTarget={{ routeId: "id-chat" }} />
      </MemoryRouter>,
    );
    await user.click(screen.getByRole("button", { name: "Connect" }));
    expect(onConnect).toHaveBeenCalledExactlyOnceWith("id-chat");

    rerender(
      <MemoryRouter>
        <UpstreamInspector
          {...props}
          connectTarget={{ unavailableReason: "Add the deployment to a route to connect to it." }}
        />
      </MemoryRouter>,
    );
    expect(screen.getByRole("button", { name: "Connect" })).toHaveAttribute(
      "aria-disabled",
      "true",
    );
  });

  it("submits the endpoint form once on Enter without saving Name metadata", async () => {
    const user = setupUser();
    const onSaveEndpoint = vi.fn().mockResolvedValue(true);
    const onSaveMetadata = vi.fn();
    render(
      <MemoryRouter>
        <UpstreamEditInspector
          metadata={{ displayName: "Upstream model" }}
          deployment={upstreamDeployment()}
          onBack={vi.fn()}
          onOpenChange={vi.fn()}
          onSaveEndpoint={onSaveEndpoint}
          onSaveMetadata={onSaveMetadata}
          open
          openerRef={createRef()}
        />
      </MemoryRouter>,
    );
    const inspector = screen.getByRole("complementary", { name: "Upstream model · Edit" });
    await waitForInspectorFocus(inspector);
    const baseUrl = within(inspector).getByLabelText("Base URL");
    await user.clear(baseUrl);
    await user.type(baseUrl, "https://updated.example/v1");
    await user.keyboard("{Enter}");

    await waitFor(() => expect(onSaveEndpoint).toHaveBeenCalledOnce());
    expect(onSaveMetadata).not.toHaveBeenCalled();
  });

  it("runs endpoint validation and focuses the first invalid field on Enter", async () => {
    const user = setupUser();
    const onSaveEndpoint = vi.fn();
    render(
      <MemoryRouter>
        <UpstreamEditInspector
          metadata={{ displayName: "Upstream model" }}
          deployment={upstreamDeployment()}
          onBack={vi.fn()}
          onOpenChange={vi.fn()}
          onSaveEndpoint={onSaveEndpoint}
          onSaveMetadata={() => Promise.resolve({ displayName: "Upstream model" })}
          open
          openerRef={createRef()}
        />
      </MemoryRouter>,
    );
    const inspector = screen.getByRole("complementary", { name: "Upstream model · Edit" });
    await waitForInspectorFocus(inspector);
    const baseUrl = within(inspector).getByLabelText("Base URL");
    await user.clear(baseUrl);
    await user.clear(within(inspector).getByLabelText("Upstream model ID"));
    await user.click(baseUrl);
    await user.keyboard("{Enter}");

    expect(onSaveEndpoint).not.toHaveBeenCalled();
    expect(within(inspector).getByText("Base URL is required.")).toBeVisible();
    expect(within(inspector).getByText("Upstream model ID is required.")).toBeVisible();
    expect(baseUrl).toHaveFocus();
  });

  it("explains invalid connection values on blur and attempted progression", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    render(
      <UpstreamDeployDialog open onOpenChange={vi.fn()} onSubmit={onSubmit} template={template} />,
    );
    const dialog = screen.getByRole("dialog", { name: "Upstream model — Deploy model" });
    const baseUrl = within(dialog).getByLabelText("Base URL");
    const modelId = within(dialog).getByLabelText("Upstream model ID");

    await user.type(baseUrl, "https://endpoint.example/v1?region=one");
    await user.tab();
    expect(await within(dialog).findByText("Base URL must not include a query.")).toBeVisible();
    expect(baseUrl).toHaveAttribute("aria-invalid", "true");
    expect(baseUrl).toHaveAccessibleDescription(
      "Address of the OpenAI-compatible server that runs the model. Base URL must not include a query.",
    );
    await user.clear(baseUrl);
    await user.type(baseUrl, "https://endpoint.example/v1");
    expect(
      within(dialog).queryByText("Base URL must not include a query."),
    ).not.toBeInTheDocument();

    await user.type(modelId, "   ");
    await user.click(within(dialog).getByRole("checkbox", { name: "Advanced" }));
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    expect(within(dialog).getByText("Upstream model ID is required.")).toBeVisible();
    expect(modelId).toHaveFocus();
    expect(within(dialog).getByText("Step 1 of 2 — Connection")).toBeVisible();
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it("preserves rapid changes without a render-phase update warning", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    const consoleError = vi.spyOn(console, "error");
    try {
      render(
        <StrictMode>
          <UpstreamDeployDialog
            open
            onOpenChange={vi.fn()}
            onSubmit={onSubmit}
            template={template}
          />
        </StrictMode>,
      );
      const dialog = screen.getByRole("dialog", { name: "Upstream model — Deploy model" });

      fireEvent.change(within(dialog).getByRole("textbox", { name: /^Base URL/ }), {
        target: { value: "https://rapid.example/v1" },
      });
      fireEvent.change(within(dialog).getByLabelText("Upstream model ID"), {
        target: { value: "first-model" },
      });
      await openDeploy(dialog, user);
      expect(within(dialog).getByLabelText("Route name")).toHaveValue("first-model");
      fireEvent.change(within(dialog).getByLabelText("Route name"), {
        target: { value: "public-route" },
      });
      await user.click(within(dialog).getByRole("button", { name: "Back" }));
      fireEvent.change(within(dialog).getByLabelText("Upstream model ID"), {
        target: { value: "second-model" },
      });
      fireEvent.change(within(dialog).getByLabelText("API key"), {
        target: { value: "fresh-secret" },
      });
      await user.click(within(dialog).getByRole("button", { name: "Next" }));
      fireEvent.change(within(dialog).getByLabelText("Deployment name"), {
        target: { value: "Rapid upstream" },
      });
      await user.click(within(dialog).getByRole("button", { name: "Deploy" }));

      expect(within(dialog).queryByText("Base URL is required.")).not.toBeInTheDocument();
      expect(onSubmit).toHaveBeenCalledWith({
        ...defaultUpstreamForm(),
        apiKey: "fresh-secret",
        baseUrl: "https://rapid.example/v1",
        name: "Rapid upstream",
        routeName: "public-route",
        upstreamModelId: "second-model",
      });
      expect(hasRenderPhaseUpdateWarning(consoleError.mock.calls)).toBe(false);
    } finally {
      consoleError.mockRestore();
    }
  });

  it("serializes every form field exactly and does not invent local fields", () => {
    const configuration = toUpstreamConfiguration(
      values({ baseUrl: " http://endpoint.example/v1 ", trustDomain: "" }),
    );
    expect(configuration).toMatchObject({
      upstreamModelId: "private-model",
      apiKey: "secret-key",
      inputCapabilities: { supportsText: true, supportsImage: true },
      endpoint: { baseUrl: "http://endpoint.example/v1", allowInsecureHttp: true },
      boundary: {
        locality: UpstreamModelLocality.PRIVATE,
        trustDomain: "endpoint.example",
        dataEgressClass: UpstreamModelDataEgressClass.PRIVATE_NETWORK,
      },
    });
    expect(configuration).not.toHaveProperty("modelId");
    expect(configuration).not.toHaveProperty("profile");
  });

  it("keeps Deploy fields ordered and minimal while normalizing a followed route name", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    render(
      <UpstreamDeployDialog open onOpenChange={vi.fn()} onSubmit={onSubmit} template={template} />,
    );

    const dialog = screen.getByRole("dialog", { name: "Upstream model — Deploy model" });
    expect(dialog).toBeVisible();
    expect(
      Array.from(dialog.querySelectorAll("label"), (label) => label.textContent?.trim()),
    ).toEqual(["Base URL", "Upstream model ID", "API key", "Advanced"]);
    expect(dialog).not.toHaveAttribute("aria-describedby");
    expect(
      within(dialog).queryByText(/requests and model inputs leave this site/i),
    ).not.toBeInTheDocument();
    expect(within(dialog).queryByRole("textbox", { name: "Name" })).not.toBeInTheDocument();
    expect(within(dialog).getByLabelText("API key")).toHaveAttribute("type", "password");
    for (const label of [
      "Model profile",
      "Engine",
      "VRAM",
      "Placement",
      "Locality",
      "Trust domain",
      "Data egress",
      "Allow insecure HTTP",
    ]) {
      expect(within(dialog).queryByLabelText(label)).not.toBeInTheDocument();
    }

    await user.type(within(dialog).getByLabelText("Base URL"), "https://endpoint.example/v1");
    await user.type(
      within(dialog).getByLabelText("Upstream model ID"),
      "DeepSeek/V4 Flash (DSPARK)",
    );
    await openDeploy(dialog, user);
    expect(within(dialog).getByLabelText("Route name")).toHaveValue("deepseek-v4-flash-dspark");
    await user.click(within(dialog).getByRole("button", { name: "Back" }));
    await user.clear(within(dialog).getByLabelText("Upstream model ID"));
    await user.type(within(dialog).getByLabelText("Upstream model ID"), "deepseek-v4-flash-dspark");
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    const route = within(dialog).getByLabelText("Route name");
    await user.clear(route);
    await user.tab();

    // An emptied route name returns to following the deployment name.
    expect(route).toHaveValue("deepseek-v4-flash-dspark");
    const name = within(dialog).getByRole("textbox", { name: "Deployment name" });
    expect(name).toHaveValue("deepseek-v4-flash-dspark");
    expect(name).toHaveAttribute("data-slot", "input");
    expect(summaryRows(dialog)).toEqual([
      ["Base URL", "https://endpoint.example/v1"],
      ["Upstream model ID", "deepseek-v4-flash-dspark"],
      ["API key", "Not configured"],
    ]);
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it("lets the route name follow the deployment name until edited and restores it with the link", async () => {
    const user = setupUser();
    render(
      <UpstreamDeployDialog open onOpenChange={vi.fn()} onSubmit={vi.fn()} template={template} />,
    );
    const dialog = screen.getByRole("dialog", { name: "Upstream model — Deploy model" });
    await user.type(within(dialog).getByLabelText("Base URL"), "https://endpoint.example/v1");
    await user.type(within(dialog).getByLabelText("Upstream model ID"), "provider/model");
    await openDeploy(dialog, user);
    const route = () => within(dialog).getByLabelText("Route name");
    const name = () => within(dialog).getByLabelText("Deployment name");

    expect(route()).toHaveValue("provider-model");
    expect(route()).toHaveClass("text-muted-foreground");
    const toggle = () => within(dialog).getByRole("button", { name: "Follow the deployment name" });
    expect(toggle()).toHaveAttribute("aria-pressed", "true");
    expect(route()).toHaveAccessibleDescription(
      "The model name callers send in requests. It follows the deployment name until you change it.",
    );
    expect(name().compareDocumentPosition(route()) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(within(dialog).queryByRole("radio", { name: "Direct" })).not.toBeInTheDocument();

    await user.clear(name());
    await user.type(name(), "Team Model");
    expect(route()).toHaveValue("team-model");

    await user.clear(route());
    await user.type(route(), "public");
    await user.type(name(), " 2");
    expect(route()).toHaveValue("public");
    expect(toggle()).toHaveAttribute("aria-pressed", "false");

    await user.click(toggle());
    expect(route()).toHaveValue("team-model-2");
    expect(toggle()).toHaveAttribute("aria-pressed", "true");
  });

  it("shows a taken custom route on the route field and blocks Deploy", async () => {
    const user = setupUser();
    render(
      <UpstreamDeployDialog
        existingRouteNames={["taken"]}
        open
        onOpenChange={vi.fn()}
        onSubmit={vi.fn()}
        template={template}
      />,
    );
    const dialog = screen.getByRole("dialog", { name: "Upstream model — Deploy model" });
    await user.type(within(dialog).getByLabelText("Base URL"), "https://endpoint.example/v1");
    await user.type(within(dialog).getByLabelText("Upstream model ID"), "free");
    await openDeploy(dialog, user);
    const route = within(dialog).getByLabelText("Route name");
    await user.clear(route);
    await user.type(route, "taken");

    expect(route).toHaveAccessibleDescription(
      'The model name callers send in requests. It follows the deployment name until you change it. An inference route named "taken" already exists.',
    );
    expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeDisabled();
  });

  it("characterizes the upstream Endpoint section inside the deployment editor", () => {
    render(
      <MemoryRouter>
        <UpstreamEditInspector
          metadata={{ displayName: "Upstream model" }}
          onSaveMetadata={() => Promise.resolve({ displayName: "Upstream model" })}
          deployment={upstreamDeployment()}
          onBack={vi.fn()}
          onOpenChange={vi.fn()}
          onSubmit={vi.fn()}
          open
          openerRef={createRef()}
        />
      </MemoryRouter>,
    );
    const inspector = screen.getByRole("complementary", { name: "Upstream model · Edit" });
    expect(
      Array.from(inspector.querySelectorAll("label"), (label) => label.textContent?.trim()),
    ).toEqual(["Name", "Base URL", "Upstream model ID", "API key"]);
    expect(within(inspector).getByRole("textbox", { name: "Name" })).toHaveAttribute(
      "id",
      "deployment-name",
    );
    expect(within(inspector).getAllByText("Name", { exact: true })).toHaveLength(1);
    expect(within(inspector).queryByRole("heading", { name: "Name" })).not.toBeInTheDocument();
    const endpointHeading = within(inspector).getByRole("heading", { name: "Endpoint" });
    const endpointSection = endpointHeading.closest("section");
    expect(endpointHeading).toBeVisible();
    expect(
      within(endpointSection!).getByLabelText("Base URL").closest('[data-slot="field"]'),
    ).toHaveClass("grid", "gap-1");
    expect(endpointSection?.querySelector(":scope > .mt-4 > .grid.gap-4")).toBeInTheDocument();
    expect(within(endpointSection!).getByRole("button", { name: "Save endpoint" })).toHaveClass(
      "mt-4",
    );
    expect(within(inspector).getByRole("button", { name: "Save" })).toBeEnabled();
    expect(within(inspector).queryByRole("button", { name: "Save name" })).not.toBeInTheDocument();
    expect(within(inspector).getByLabelText("API key")).toHaveAttribute(
      "autoComplete",
      "new-password",
    );
    expect(within(inspector).getByLabelText("API key")).toHaveAttribute("type", "password");
  });

  it("validates blank Base URL before model ID, focuses it, and skips the endpoint RPC", async () => {
    const user = setupUser();
    const onSaveEndpoint = vi.fn();
    render(
      <MemoryRouter>
        <UpstreamEditInspector
          metadata={{ displayName: "Upstream model" }}
          onSaveMetadata={() => Promise.resolve({ displayName: "Upstream model" })}
          deployment={upstreamDeployment()}
          onBack={vi.fn()}
          onOpenChange={vi.fn()}
          onSaveEndpoint={onSaveEndpoint}
          open
          openerRef={createRef()}
        />
      </MemoryRouter>,
    );

    const inspector = screen.getByRole("complementary", { name: "Upstream model · Edit" });
    await waitForInspectorFocus(inspector);
    const baseUrl = within(inspector).getByLabelText("Base URL");
    await user.clear(baseUrl);
    await user.clear(within(inspector).getByLabelText("Upstream model ID"));
    await user.click(within(inspector).getByRole("button", { name: "Save endpoint" }));

    expect(onSaveEndpoint).not.toHaveBeenCalled();
    expect(within(inspector).getByText("Base URL is required.")).toBeVisible();
    expect(within(inspector).getByText("Upstream model ID is required.")).toBeVisible();
    expect(baseUrl).toHaveFocus();
  });

  it("handles rejected metadata saves without losing drafts or navigation", async () => {
    const user = setupUser();
    const onSaveMetadata = vi.fn().mockRejectedValue(new Error("Name could not be saved."));
    const router = createMemoryRouter(
      [
        {
          path: "*",
          element: (
            <PanelNavigationProvider>
              <UpstreamEditInspector
                metadata={{ displayName: "Upstream model" }}
                deployment={upstreamDeployment()}
                onBack={vi.fn()}
                onOpenChange={vi.fn()}
                onSaveMetadata={onSaveMetadata}
                open
                openerRef={createRef()}
              />
              <LocationProbe />
            </PanelNavigationProvider>
          ),
        },
      ],
      { initialEntries: ["/models?panel=models%2Fedit%3Adeployment-1"] },
    );
    render(<RouterProvider router={router} />);

    const inspector = screen.getByRole("complementary", { name: "Upstream model · Edit" });
    await waitForInspectorFocus(inspector);
    const baseUrl = within(inspector).getByLabelText("Base URL");
    fireEvent.change(baseUrl, { target: { value: "https://draft.example/v1" } });
    const name = within(inspector).getByRole("textbox", { name: "Name" });
    await user.clear(name);
    await user.type(name, "Rejected name");
    await user.click(within(inspector).getByRole("button", { name: "Save" }));

    await waitFor(() => expect(onSaveMetadata).toHaveBeenCalledTimes(1));
    expect(within(inspector).getByRole("alert")).toHaveTextContent("Name could not be saved.");
    expect(name).toHaveValue("Rejected name");
    expect(baseUrl).toHaveValue("https://draft.example/v1");
    expect(within(inspector).getByRole("button", { name: "Save" })).toBeEnabled();
    expect(screen.getByTestId("location")).toHaveTextContent(
      "/models?panel=models%2Fedit%3Adeployment-1",
    );
  });

  it("submits partial metadata while preserving the Endpoint draft", async () => {
    const user = setupUser();
    const onSaveMetadata = vi.fn().mockResolvedValue({ displayName: "Accepted name" });
    render(
      <MemoryRouter>
        <UpstreamEditInspector
          metadata={{ displayName: "Upstream model" }}
          deployment={upstreamDeployment()}
          onBack={vi.fn()}
          onOpenChange={vi.fn()}
          onSaveMetadata={onSaveMetadata}
          open
          openerRef={createRef()}
        />
      </MemoryRouter>,
    );

    const inspector = screen.getByRole("complementary", { name: "Upstream model · Edit" });
    const baseUrl = within(inspector).getByLabelText("Base URL");
    fireEvent.change(baseUrl, { target: { value: "https://draft.example/v1" } });
    await waitFor(() => expect(baseUrl).toHaveValue("https://draft.example/v1"));
    const name = within(inspector).getByRole("textbox", { name: "Name" });
    await user.clear(name);
    await user.type(name, "Accepted name");
    await user.click(within(inspector).getByRole("button", { name: "Save" }));

    expect(onSaveMetadata).toHaveBeenCalledWith({
      fields: ["displayName"],
      metadata: { displayName: "Accepted name" },
    });
    await waitFor(() =>
      expect(within(inspector).getByRole("status")).toHaveTextContent(
        "Name saved. Endpoint changes are still unsaved.",
      ),
    );
    expect(baseUrl).toHaveValue("https://draft.example/v1");
  });

  it("synchronizes clean projected fields while retaining dirty drafts against advanced baselines", async () => {
    const initial = create(ModelDeploymentSchema, {
      ...upstreamDeployment(),
      modelDeploymentName: "Initial name",
    });
    const { rerender } = render(
      <MemoryRouter>
        <UpstreamEditInspector
          metadata={{ displayName: "Initial name" }}
          onSaveMetadata={() => Promise.resolve({ displayName: "Initial name" })}
          deployment={initial}
          onBack={vi.fn()}
          onOpenChange={vi.fn()}
          open
          openerRef={createRef()}
        />
      </MemoryRouter>,
    );
    const inspector = screen.getByRole("complementary", { name: "Initial name · Edit" });
    const name = within(inspector).getByRole("textbox", { name: "Name" });
    const baseUrl = within(inspector).getByLabelText("Base URL");

    rerender(
      <MemoryRouter>
        <UpstreamEditInspector
          metadata={{ displayName: "Projected name" }}
          onSaveMetadata={() => Promise.resolve({ displayName: "Projected name" })}
          deployment={create(ModelDeploymentSchema, {
            ...initial,
            modelDeploymentName: "Projected name",
            upstreamDesiredConfiguration: create(UpstreamModelDesiredConfigurationSchema, {
              upstreamModelId: "private-model",
              endpoint: create(OpenAiCompatibleEndpointConfigurationSchema, {
                baseUrl: "https://projected.example/v1",
              }),
            }),
          })}
          onBack={vi.fn()}
          onOpenChange={vi.fn()}
          open
          openerRef={createRef()}
        />
      </MemoryRouter>,
    );
    await waitFor(() => expect(name).toHaveValue("Projected name"));
    expect(baseUrl).toHaveValue("https://projected.example/v1");

    const user = setupUser();
    await user.clear(name);
    await user.type(name, "Unsaved name");
    rerender(
      <MemoryRouter>
        <UpstreamEditInspector
          metadata={{ displayName: "New authoritative name" }}
          onSaveMetadata={() => Promise.resolve({ displayName: "New authoritative name" })}
          deployment={create(ModelDeploymentSchema, {
            ...initial,
            modelDeploymentName: "New authoritative name",
            upstreamDesiredConfiguration: create(UpstreamModelDesiredConfigurationSchema, {
              upstreamModelId: "private-model",
              endpoint: create(OpenAiCompatibleEndpointConfigurationSchema, {
                baseUrl: "https://new-authoritative.example/v1",
              }),
            }),
          })}
          onBack={vi.fn()}
          onOpenChange={vi.fn()}
          open
          openerRef={createRef()}
        />
      </MemoryRouter>,
    );
    await waitFor(() => expect(name).toHaveValue("Unsaved name"));
    await waitFor(() => expect(baseUrl).toHaveValue("https://new-authoritative.example/v1"));
  });

  it("disables the route name for Hidden deployments and submits the hidden exposure", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    render(
      <UpstreamDeployDialog open onOpenChange={vi.fn()} onSubmit={onSubmit} template={template} />,
    );

    const dialog = screen.getByRole("dialog", { name: "Upstream model — Deploy model" });
    await user.type(within(dialog).getByLabelText("Base URL"), "https://endpoint.example/v1");
    await user.type(within(dialog).getByLabelText("Upstream model ID"), "private-model");
    await user.click(within(dialog).getByRole("checkbox", { name: "Advanced" }));
    await openDeploy(dialog, user);
    const route = within(dialog).getByLabelText("Route name");
    await user.clear(route);
    await user.type(route, "Bad Name!");
    await user.tab();
    expect(route).toHaveAttribute("aria-invalid", "true");

    await user.click(within(dialog).getByRole("radio", { name: "Hidden" }));
    expect(route).toBeDisabled();
    expect(route).not.toHaveAttribute("aria-invalid");
    expect(route).toHaveAccessibleDescription(
      "The model name callers send in requests. It follows the deployment name until you change it. Hidden: no route of its own. Reachable only through other routes.",
    );
    expect(within(dialog).queryByText(/Route name must start/)).not.toBeInTheDocument();

    // Basic mode keeps showing the choice while the deployment is hidden.
    await user.click(within(dialog).getByRole("checkbox", { name: "Advanced" }));
    expect(within(dialog).getByRole("radio", { name: "Hidden" })).toBeChecked();
    expect(
      within(dialog).queryByRole("button", { name: "Follow the deployment name" }),
    ).not.toBeInTheDocument();

    await user.click(within(dialog).getByRole("button", { name: "Deploy" }));

    expect(onSubmit).toHaveBeenCalledOnce();
    expect(onSubmit.mock.calls[0][0]).toMatchObject({ exposure: "hidden" });
  });

  it("disables the exposure choice while the deployment is pending", async () => {
    const user = setupUser();
    const props = { open: true, onOpenChange: vi.fn(), onSubmit: vi.fn(), template };
    const { rerender } = render(<UpstreamDeployDialog {...props} />);
    const dialog = screen.getByRole("dialog", { name: "Upstream model — Deploy model" });
    await user.type(within(dialog).getByLabelText("Base URL"), "https://endpoint.example/v1");
    await user.type(within(dialog).getByLabelText("Upstream model ID"), "private-model");
    await user.click(within(dialog).getByRole("checkbox", { name: "Advanced" }));
    await openDeploy(dialog, user);
    const direct = within(dialog).getByRole("radio", { name: "Direct" });
    const hidden = within(dialog).getByRole("radio", { name: "Hidden" });
    expect(direct).not.toHaveAttribute("aria-disabled", "true");

    rerender(<UpstreamDeployDialog {...props} pending />);
    expect(direct).toHaveAttribute("aria-disabled", "true");
    await user.click(hidden);
    expect(direct).toBeChecked();
    expect(hidden).not.toBeChecked();
  });

  it("is full screen on small viewports and a centered card from sm with internal scrolling", () => {
    render(
      <UpstreamDeployDialog open onOpenChange={vi.fn()} onSubmit={vi.fn()} template={template} />,
    );

    const content = document.querySelector('[data-slot="dialog-content"]');
    expect(content).toHaveClass(
      "inset-0",
      "h-dvh",
      "max-h-none",
      "max-w-none",
      "sm:max-h-[calc(100dvh-2rem)]",
      "sm:w-[calc(100vw-2rem)]",
      "sm:max-w-[48rem]",
      "grid-rows-[auto_minmax(0,1fr)_auto]",
      "overflow-hidden",
    );
    expect(screen.getByTestId("deployment-dialog-body")).toHaveClass("min-h-0", "overflow-y-auto");
  });

  it("keeps the Deploy dialog open and ignores dismissal while pending", async () => {
    const user = setupUser();
    const onOpenChange = vi.fn();
    const onSubmit = vi.fn();
    const { rerender } = render(
      <UpstreamDeployDialog
        open
        onOpenChange={onOpenChange}
        onSubmit={onSubmit}
        template={template}
      />,
    );
    const dialog = screen.getByRole("dialog", { name: "Upstream model — Deploy model" });
    await user.type(within(dialog).getByLabelText("Base URL"), "https://endpoint.example/v1");
    await user.type(within(dialog).getByLabelText("Upstream model ID"), "private-model");
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    rerender(
      <UpstreamDeployDialog
        open
        onOpenChange={onOpenChange}
        onSubmit={onSubmit}
        pending
        template={template}
      />,
    );

    expect(within(dialog).getByRole("button", { name: "Deploying…" })).toBeDisabled();
    expect(within(dialog).getByRole("button", { name: "Cancel" })).toBeDisabled();
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    expect(onOpenChange).not.toHaveBeenCalled();
  });

  it("guards dirty upstream Edit stack navigation", async () => {
    const user = setupUser();
    const editLocation =
      "/models?panel=models%2Fdeployment%3Adeployment-1&panel=models%2Fedit%3Adeployment-1";
    const router = createMemoryRouter(
      [
        {
          path: "*",
          element: (
            <PanelNavigationProvider>
              <GuardedUpstreamEdit />
            </PanelNavigationProvider>
          ),
        },
      ],
      { initialEntries: [editLocation] },
    );
    render(<RouterProvider router={router} />);

    const baseUrl = screen.getByRole("textbox", { name: "Base URL" });
    fireEvent.change(baseUrl, { target: { value: "https://changed.example/v1" } });
    await user.click(screen.getByRole("button", { name: "Back to model deployment" }));
    expect(screen.getByRole("dialog", { name: "Discard changes?" })).toBeVisible();
    expect(router.state.location.pathname + router.state.location.search).toBe(editLocation);
    await user.click(screen.getByRole("button", { name: "Discard changes" }));
    expect(router.state.location.search).toBe("?panel=models%2Fdeployment%3Adeployment-1");
  });

  it("disables upstream Edit stack navigation with an accessible pending explanation", () => {
    render(
      <MemoryRouter>
        <PanelParentContext.Provider
          value={{ page: "models", screen: "deployment", resourceId: "deployment-1" }}
        >
          <UpstreamEditInspector
            metadata={{ displayName: "Upstream model" }}
            onSaveMetadata={() => Promise.resolve({ displayName: "Upstream model" })}
            deployment={upstreamDeployment()}
            onBack={vi.fn()}
            onOpenChange={vi.fn()}
            onSubmit={vi.fn()}
            open
            openerRef={createRef()}
            pending
          />
        </PanelParentContext.Provider>
      </MemoryRouter>,
    );

    expect(screen.getByRole("button", { name: "Saving endpoint…" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Back to model deployment" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Close inspector" })).toBeDisabled();
    expect(
      screen.getByRole("button", { name: "Back to model deployment" }),
    ).toHaveAccessibleDescription("Saving changes. Inspector navigation is unavailable.");
  });

  it("does not return after teardown settles a pending final-clean Name save", async () => {
    const user = setupUser();
    const save = deferredSave();
    const editorLocation =
      "/models?panel=models%2Fdeployment%3Adeployment-1&panel=models%2Fedit%3Adeployment-1";
    renderTerminalUpstreamEdit(save);

    const inspector = screen.getByRole("complementary", { name: "Upstream model · Edit" });
    await waitForInspectorFocus(inspector);
    const name = within(inspector).getByRole("textbox", { name: "Name" });
    await user.clear(name);
    await user.type(name, "Accepted name");
    await user.click(within(inspector).getByRole("button", { name: "Save" }));
    expect(screen.getByRole("button", { name: "Saving…" })).toBeDisabled();

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
    expect(screen.getByTestId("location")).toHaveTextContent(editorLocation);
    expect(within(inspector).getByRole("button", { name: "Save" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Close inspector" })).toBeEnabled();

    await user.click(screen.getByRole("button", { name: "Back to model deployment" }));
    await waitFor(() =>
      expect(screen.getByTestId("location")).toHaveTextContent(
        "/models?panel=models%2Fdeployment%3Adeployment-1",
      ),
    );
  });

  it("does not return after teardown settles a pending final-clean Endpoint save", async () => {
    const user = setupUser();
    const save = deferredSave();
    const editorLocation =
      "/models?panel=models%2Fdeployment%3Adeployment-1&panel=models%2Fedit%3Adeployment-1";
    renderTerminalUpstreamEdit(save);

    const inspector = screen.getByRole("complementary", { name: "Upstream model · Edit" });
    const baseUrl = within(inspector).getByLabelText("Base URL");
    fireEvent.change(baseUrl, { target: { value: "https://accepted.example/v1" } });
    await waitFor(() => expect(baseUrl).toHaveValue("https://accepted.example/v1"));
    await user.click(within(inspector).getByRole("button", { name: "Save endpoint" }));
    expect(screen.getByRole("button", { name: "Saving endpoint…" })).toBeDisabled();

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
    expect(screen.getByTestId("location")).toHaveTextContent(editorLocation);
    expect(within(inspector).getByRole("button", { name: "Save endpoint" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Close inspector" })).toBeEnabled();

    await user.click(screen.getByRole("button", { name: "Back to model deployment" }));
    await waitFor(() =>
      expect(screen.getByTestId("location")).toHaveTextContent(
        "/models?panel=models%2Fdeployment%3Adeployment-1",
      ),
    );
    expect(screen.queryByRole("dialog", { name: "Discard changes?" })).not.toBeInTheDocument();
  });

  it("clears an unsaveable Endpoint draft after a partial Name save observes teardown", async () => {
    const user = setupUser();
    const save = deferredSave();
    renderTerminalUpstreamEdit(save);

    const inspector = screen.getByRole("complementary", { name: "Upstream model · Edit" });
    const baseUrl = within(inspector).getByLabelText("Base URL");
    fireEvent.change(baseUrl, { target: { value: "https://draft.example/v1" } });
    await waitFor(() => expect(baseUrl).toHaveValue("https://draft.example/v1"));
    const name = within(inspector).getByRole("textbox", { name: "Name" });
    await user.clear(name);
    await user.type(name, "Accepted name");
    await user.click(within(inspector).getByRole("button", { name: "Save" }));
    await act(async () => {
      save.resolve({ displayName: "Accepted name" });
      await Promise.resolve();
    });
    await waitFor(() =>
      expect(screen.getByText("Name saved. Endpoint changes are still unsaved.")).toBeVisible(),
    );
    expect(baseUrl).toHaveValue("https://draft.example/v1");

    await user.click(screen.getByRole("button", { name: "Observe teardown" }));
    expect(
      screen.getByText("This deployment is tearing down and can no longer be edited."),
    ).toBeVisible();
    expect(within(inspector).getByRole("button", { name: "Save" })).toBeDisabled();
    expect(within(inspector).getByRole("button", { name: "Save endpoint" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Back to model deployment" })).toBeEnabled();

    await user.click(screen.getByRole("button", { name: "Back to model deployment" }));
    await waitFor(() =>
      expect(screen.getByTestId("location")).toHaveTextContent(
        "/models?panel=models%2Fdeployment%3Adeployment-1",
      ),
    );
    expect(screen.queryByRole("dialog", { name: "Discard changes?" })).not.toBeInTheDocument();
  });

  it("keeps the stored API key out of the Edit draft while preserving blank replacement semantics", () => {
    const deployment = upstreamDeployment();
    expect(defaultUpstreamForm(deployment).apiKey).toBe("");
    expect(toUpstreamConfiguration(values({ apiKey: "" })).apiKey).toBe("");
  });

  it("derives the hidden trust domain from the endpoint for a new deployment", () => {
    expect(
      toUpstreamConfiguration(
        values({ baseUrl: "https://models.example:8443/v1", trustDomain: "" }),
      ),
    ).toMatchObject({ boundary: { trustDomain: "models.example" } });
  });

  it("keeps an edited route name when returning to Connection", async () => {
    const user = setupUser();
    render(
      <UpstreamDeployDialog open onOpenChange={vi.fn()} onSubmit={vi.fn()} template={template} />,
    );

    const dialog = screen.getByRole("dialog", { name: "Upstream model — Deploy model" });
    await user.type(within(dialog).getByLabelText("Base URL"), "https://endpoint.example/v1");
    let upstream = within(dialog).getByLabelText("Upstream model ID");
    await user.type(upstream, "first-model");
    await openDeploy(dialog, user);
    let dotcore = within(dialog).getByLabelText("Route name");
    await user.clear(dotcore);
    await user.type(dotcore, "public-route");
    await user.click(within(dialog).getByRole("button", { name: "Back" }));
    upstream = within(dialog).getByLabelText("Upstream model ID");
    await user.clear(upstream);
    await user.type(upstream, "second-model");
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    dotcore = within(dialog).getByLabelText("Route name");
    expect(dotcore).toHaveValue("public-route");
  });

  it("focuses the upstream deployment inspector heading when it opens", async () => {
    render(
      <UpstreamInspector
        metricsClient={idleMetricsClient()}
        deployment={upstreamDeployment()}
        open
        onEdit={vi.fn()}
        onUnload={vi.fn()}
        onOpenChange={vi.fn()}
        openerRef={createRef()}
      />,
    );

    const panel = screen.getByRole("complementary", { name: "Upstream model" });
    await waitFor(() =>
      expect(within(panel).getByRole("heading", { name: "Upstream model" })).toHaveFocus(),
    );
  });

  it("shows the deployment's six request cards from its own attempts", async () => {
    const watchMetrics = vi.fn(
      staticWatch((query) =>
        queryResult({
          series:
            query?.metric === "router.attempts"
              ? [metricSeries({}, [counterPoint(0, 0, false, 4)])]
              : query?.metric === "router.attempt.ttft"
                ? [metricSeries({}, [distributionPoint(0, {}, false, { count: 2, sum: 0.5 })])]
                : [],
          stepCount: 60,
          stepSeconds: 15,
        }),
      ),
    );
    render(
      <UpstreamInspector
        deployment={upstreamDeployment()}
        metricsClient={metricsClientWith(watchMetrics)}
        open
        onEdit={vi.fn()}
        onUnload={vi.fn()}
        onOpenChange={vi.fn()}
        openerRef={createRef()}
      />,
    );

    const requests = screen.getByRole("region", { name: "Requests" });
    expect(
      await within(requests).findByRole("button", { name: /^Time to first token250 ms/ }),
    ).toBeVisible();
    expect(
      within(requests)
        .getAllByRole("button")
        .map((card) => card.querySelector('[data-slot="metric-card-label"]')?.textContent),
    ).toEqual([
      "Requests",
      "Processed tokens",
      "Generated tokens",
      "Prompt processing",
      "Time to first token",
      "Token generation",
    ]);
    expect(within(requests).getByRole("button", { name: /^Requests4/ })).toBeVisible();
    expect(watchMetrics.mock.calls[0][0].queries?.[0].query?.matchers).toEqual({
      deployment: "deployment-1",
    });
  });

  it("makes Chat primary inside the console layout", async () => {
    const user = setupUser();
    const openChat = vi.fn();
    render(
      <ChatOpenerContext.Provider value={openChat}>
        <UpstreamInspector
          metricsClient={idleMetricsClient()}
          chatTarget={{ label: "Chat", mode: "chat", route: "remote-gemma" }}
          deployment={upstreamDeployment()}
          open
          onEdit={vi.fn()}
          onUnload={vi.fn()}
          onOpenChange={vi.fn()}
          openerRef={createRef()}
        />
      </ChatOpenerContext.Provider>,
    );

    expect(await inspectorActionList(user)).toEqual({
      buttons: ["Chat", "More actions"],
      menu: ["Edit", "Teardown…"],
    });
    await user.click(screen.getByRole("button", { name: "Chat" }));
    expect(openChat).toHaveBeenCalledWith("remote-gemma", "chat");
  });

  it("keeps upstream teardown confirmation pending, actionable, and retryable", async () => {
    const user = setupUser();
    const dependencyError =
      'Cannot teardown the model deployment because application deployment "Open WebUI" (application-one) uses inference route "Upstream route" (route-one), which would be removed with the model deployment. Teardown the application and wait for it to be removed, then try again.';
    const onUnload = vi.fn().mockResolvedValueOnce(false).mockResolvedValueOnce(true);
    const onOpenChange = vi.fn();
    const { rerender } = render(
      <UpstreamInspector
        metricsClient={idleMetricsClient()}
        deployment={upstreamDeployment()}
        error="Update endpoint failed."
        open
        onEdit={vi.fn()}
        onUnload={onUnload}
        onOpenChange={onOpenChange}
        openerRef={createRef()}
      />,
    );

    expect(await inspectorActionList(user)).toEqual({
      buttons: ["More actions"],
      menu: ["Edit", "Teardown…"],
    });
    await chooseMoreAction(user, "Teardown…");
    const confirmation = screen.getByRole("dialog", { name: "Teardown Upstream model?" });
    expect(within(confirmation).queryByText("Update endpoint failed.")).not.toBeInTheDocument();
    expect(within(confirmation).getByText(/deployment-1/)).toHaveTextContent(
      "This removes upstream model deployment deployment-1.",
    );
    await waitFor(() =>
      expect(within(confirmation).getByRole("button", { name: "Cancel" })).toHaveFocus(),
    );
    await user.keyboard("{Escape}");
    expect(
      screen.queryByRole("dialog", { name: "Teardown Upstream model?" }),
    ).not.toBeInTheDocument();
    expect(onOpenChange).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "More actions" })).toHaveFocus();

    await chooseMoreAction(user, "Teardown…");
    await user.click(
      within(screen.getByRole("dialog", { name: "Teardown Upstream model?" })).getByRole("button", {
        name: "Teardown",
      }),
    );
    expect(onUnload).toHaveBeenCalledOnce();

    rerender(
      <UpstreamInspector
        metricsClient={idleMetricsClient()}
        deployment={upstreamDeployment()}
        open
        onEdit={vi.fn()}
        onUnload={onUnload}
        onOpenChange={onOpenChange}
        openerRef={createRef()}
        pending
      />,
    );
    const pendingConfirmation = screen.getByRole("dialog", { name: "Teardown Upstream model?" });
    expect(
      within(pendingConfirmation).getByRole("button", { name: "Tearing down…" }),
    ).toBeDisabled();
    expect(within(pendingConfirmation).getByRole("button", { name: "Cancel" })).toBeDisabled();
    await user.keyboard("{Escape}");
    expect(screen.getByRole("dialog", { name: "Teardown Upstream model?" })).toBeVisible();

    rerender(
      <UpstreamInspector
        metricsClient={idleMetricsClient()}
        deployment={upstreamDeployment()}
        error={dependencyError}
        teardownError={dependencyError}
        open
        onEdit={vi.fn()}
        onUnload={onUnload}
        onOpenChange={onOpenChange}
        openerRef={createRef()}
      />,
    );
    const failedConfirmation = screen.getByRole("dialog", { name: "Teardown Upstream model?" });
    expect(within(failedConfirmation).getByRole("alert")).toHaveTextContent(dependencyError);
    expect(screen.getAllByText(dependencyError)).toHaveLength(1);
    expect(within(failedConfirmation).getByRole("button", { name: "Try again" })).toBeEnabled();

    await user.click(within(failedConfirmation).getByRole("button", { name: "Cancel" }));
    expect(
      screen.queryByRole("dialog", { name: "Teardown Upstream model?" }),
    ).not.toBeInTheDocument();
    expect(screen.getByRole("alert")).toHaveTextContent(dependencyError);
    await waitFor(() => expect(screen.getByRole("button", { name: "More actions" })).toHaveFocus());

    await chooseMoreAction(user, "Teardown…");
    await user.click(
      within(screen.getByRole("dialog", { name: "Teardown Upstream model?" })).getByRole("button", {
        name: "Try again",
      }),
    );
    await waitFor(() =>
      expect(
        screen.queryByRole("dialog", { name: "Teardown Upstream model?" }),
      ).not.toBeInTheDocument(),
    );
    expect(onUnload).toHaveBeenCalledTimes(2);
  });

  it("shows boundary warnings and source-neutral public connection identity", () => {
    const deployment = upstreamDeployment();
    const onEdit = vi.fn();
    render(
      <MemoryRouter>
        <UpstreamInspector
          metricsClient={idleMetricsClient()}
          deployment={deployment}
          open
          onEdit={onEdit}
          onUnload={vi.fn()}
          onOpenChange={vi.fn()}
          openerRef={createRef()}
        />
      </MemoryRouter>,
    );

    const panel = screen.getByRole("complementary", { name: "Upstream model" });
    expect(panel).toBeVisible();
    // expectResourceMarker already asserts the single marker, its aria-hidden state, and its colour.
    expectResourceMarker(inspectorTitle(panel), deployment.modelDeploymentId);

    const sections = screen
      .getByTestId("inspector-body")
      .querySelectorAll(":scope > .inspector-sections > section");
    expect(Array.from(sections, (section) => section.querySelector("h2")?.textContent)).toEqual([
      "Deployment",
      "Source",
      "Requests",
    ]);

    // The identity values render at the root now, rather than behind a Details sub-screen.
    const identity = screen.getByRole("region", { name: "Deployment" });
    expect(within(identity).getByText("Deployment ID")).toBeVisible();
    expect(within(identity).getByText("deployment-1")).toBeVisible();
    expect(within(identity).getByText("No route configured.")).toBeVisible();

    const source = screen.getByRole("region", { name: "Source" });
    expect(within(source).getByText("External")).toBeVisible();
    expect(within(source).getByText("Upstream model")).toBeVisible();
    expect(within(source).getByText("private-model")).toBeVisible();
    expect(within(source).getByText("https://endpoint.example/v1")).toBeVisible();

    // An upstream deployment still exposes no placement, no connect affordance, no route-manager
    // vocabulary, and no sub-screen.
    expect(screen.queryByText(/VRAM|Placement|Engine|model profile/i)).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Connect" })).not.toBeInTheDocument();
    expect(screen.queryByText("Route-manager routes")).not.toBeInTheDocument();
    expect(screen.queryByText("None")).not.toBeInTheDocument();
    expect(screen.queryByRole("region", { name: "Details" })).not.toBeInTheDocument();
  });

  it("marks containing route links with the route color", () => {
    render(
      <MemoryRouter>
        <UpstreamInspector
          metricsClient={idleMetricsClient()}
          containingRoutes={[{ href: "/routes", inferenceRouteId: "route-a", name: "chat" }]}
          deployment={upstreamDeployment()}
          open
          onEdit={vi.fn()}
          onUnload={vi.fn()}
          onOpenChange={vi.fn()}
          openerRef={createRef()}
        />
      </MemoryRouter>,
    );

    expectResourceMarker(screen.getByRole("link", { name: "chat" }), "route-a");
  });
});
