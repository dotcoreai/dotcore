import { clone, create } from "@bufbuild/protobuf";
import { act, render as renderUi, screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import type { ReactElement } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  ExecutionTargetsWatch,
  executionTargetsWatch,
} from "@/features/deployments/execution-targets-watch-test-support";
import {} from "@/gen/dotcore/application/v1/service_pb";
import {
  ApplicationConfigurationBindingSchema,
  ApplicationDeploymentInputSchema,
  ApplicationGraphSchema,
  ApplicationSchema,
  ApplicationServiceSchema,
} from "@/gen/dotcore/catalog/v1/application_pb";
import {
  InferenceRequestKind,
  ModelInputModality,
} from "@/gen/dotcore/catalog/v1/model_capabilities_pb";
import {
  ParameterDefinitionSchema,
  ParameterValueSchema,
  ParameterVisibility,
} from "@/gen/dotcore/catalog/v1/parameter_pb";
import {
  InferenceRouteAvailabilitySchema,
  InferenceRouteSchema,
  InferenceRouteUnavailabilityReason,
  InferenceRouteViewSchema,
} from "@/gen/dotcore/inference/v1/routes_pb";
import {
  ConfiguredExecutionTargetSchema,
  ExecutionTargetWorkloadKind,
  NodeObservationState,
} from "@/gen/dotcore/site/v1/service_pb";
import type { FakeResourceWatch } from "@/test/fake-resource-watch";
import { expectResourceMarker, inspectorTitle } from "@/test/resource-marker";
import { setupUser } from "@/test/user";

import { ApplicationDeployDialog } from "./application-deploy-dialog";

const application = create(ApplicationSchema, {
  applicationId: "open-webui",
  displayName: "OpenWebUI",
  version: "0.6.0",
});

// Turns Advanced on and moves from the first step to Target.
async function openAdvanced(user: UserEvent, scope: Pick<typeof screen, "getByRole"> = screen) {
  await user.click(scope.getByRole("checkbox", { name: "Advanced" }));
  await user.click(scope.getByRole("button", { name: "Next" }));
}

// Moves from the first step to Deploy in basic mode.
async function openDeploy(user: UserEvent) {
  await user.click(screen.getByRole("button", { name: "Next" }));
  expect(screen.getByText("Step 2 of 2 — Deploy")).toBeVisible();
}

// A route-less application without basic parameters opens on Deploy, its only basic step.
function expectDeployOnly() {
  expect(screen.queryByRole("list", { name: "Deployment progress" })).not.toBeInTheDocument();
  expect(screen.getByRole("textbox", { name: "Deployment name" })).toBeVisible();
}

/** Label and value of each Deploy summary row, in display order. */
/** Renders under a resource watch, which answers no execution targets unless a test gives one. */
function render(
  ui: ReactElement,
  { watch = executionTargetsWatch() }: { watch?: FakeResourceWatch } = {},
) {
  const wrap = (element: ReactElement) => (
    <ExecutionTargetsWatch watch={watch}>{element}</ExecutionTargetsWatch>
  );
  const view = renderUi(wrap(ui));

  return { ...view, rerender: (next: ReactElement) => view.rerender(wrap(next)) };
}

function summaryRows() {
  const list = screen.getByRole("region", { name: "Summary" }).querySelector("dl")!;
  const labels = within(list).getAllByRole("term");
  const values = within(list).getAllByRole("definition");
  return labels.map((label, index) => [label.textContent, values[index]?.textContent]);
}

/** The step labels of the deployment progress, in order. */
function stepLabels() {
  return Array.from(
    screen.getByRole("list", { name: "Deployment progress" }).querySelectorAll("[data-step-label]"),
    (label) => label.textContent,
  );
}

function applicationTarget(id: string, available = true) {
  return create(ConfiguredExecutionTargetSchema, {
    displayName: "Docker applications",
    executionTargetId: id,
    siteNodeId: id,
    workloadKind: ExecutionTargetWorkloadKind.APPLICATION,
    siteManager: true,
    availability: {
      connectionState: NodeObservationState.CONNECTED,
      runtimeAvailability: {
        availability: available
          ? { case: "available", value: {} }
          : { case: "unavailable", value: { code: "RUNTIME_UNAVAILABLE", message: "Offline" } },
      },
    },
  });
}

function remoteApplicationTarget(
  id: string,
  siteNodeId = "worker",
  connectionState = NodeObservationState.CONNECTED,
  displayName = "Docker applications",
) {
  return create(ConfiguredExecutionTargetSchema, {
    displayName,
    executionTargetId: id,
    siteNodeId,
    siteNodeDisplayName: `Name of ${siteNodeId}`,
    platformLabel: "linux/arm64",
    workloadKind: ExecutionTargetWorkloadKind.APPLICATION,
    availability: {
      connectionState,
      runtimeAvailability: { availability: { case: "available", value: {} } },
    },
  });
}

function setViewportWidth(width: number) {
  Object.defineProperty(window, "innerWidth", { configurable: true, value: width });
  window.dispatchEvent(new Event("resize"));
}

afterEach(() => {
  vi.restoreAllMocks();
  setViewportWidth(1024);
});

/** The accessible names of the inspector body's sections, in rendered order. */
/** A route the route manager reports serving text through one ready primary member. */
function servingRoute(
  inferenceRouteId: string,
  name: string,
  generation: bigint,
  requestKinds: InferenceRequestKind[] = [InferenceRequestKind.CHAT],
) {
  return create(InferenceRouteViewSchema, {
    requestKinds,
    route: create(InferenceRouteSchema, {
      inferenceRouteId,
      name,
      enabled: true,
      generation,
      fallbackGroups: [{ modelDeploymentIds: [`${inferenceRouteId}-model`] }],
    }),
    availability: create(InferenceRouteAvailabilitySchema, {
      available: true,
      availableInputModalities: [ModelInputModality.TEXT],
      fallbackGroups: [
        {
          fallbackGroupIndex: 0,
          members: [
            {
              modelDeploymentId: `${inferenceRouteId}-model`,
              eligible: true,
              availableInputModalities: [ModelInputModality.TEXT],
            },
          ],
        },
      ],
    }),
  });
}

/** The same route with no eligible member, as the route manager reports it. */
function unservedRoute(view: ReturnType<typeof servingRoute>) {
  return create(InferenceRouteViewSchema, {
    requestKinds: view.requestKinds,
    route: view.route,
    availability: create(InferenceRouteAvailabilitySchema, {
      available: false,
      exclusionReasons: [InferenceRouteUnavailabilityReason.NO_ELIGIBLE_TEXT_MEMBER],
    }),
  });
}

/** The same route disabled by an administrator. */
function disabledRoute(view: ReturnType<typeof servingRoute>) {
  const route = clone(InferenceRouteSchema, view.route!);
  route.enabled = false;
  return create(InferenceRouteViewSchema, {
    requestKinds: view.requestKinds,
    route,
    availability: create(InferenceRouteAvailabilitySchema, {
      available: false,
      exclusionReasons: [InferenceRouteUnavailabilityReason.DISABLED],
    }),
  });
}

describe("Application deployment dialog", () => {
  const machine = applicationTarget("local");
  const appWithOptionalRoute = create(ApplicationSchema, {
    ...application,
    graph: create(ApplicationGraphSchema, {
      services: [
        create(ApplicationServiceSchema, {
          serviceName: "web",
          configurationBindings: [
            create(ApplicationConfigurationBindingSchema, {
              bindingName: "route",
              input: create(ApplicationDeploymentInputSchema, {
                kind: { case: "inferenceRoute", value: {} },
              }),
            }),
          ],
        }),
      ],
    }),
  });

  it("lists every route with its health and deploys unlinked on stable defaults", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    const primary = servingRoute("route-a", "primary", 3n);
    render(
      <ApplicationDeployDialog
        application={appWithOptionalRoute}
        onSubmit={onSubmit}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={[
          applicationTarget("machine-z"),
          applicationTarget("machine-b"),
          applicationTarget("machine-a"),
        ]}
        routes={[
          disabledRoute(servingRoute("route-disabled", "retired", 1n)),
          primary,
          unservedRoute(servingRoute("route-idle", "idle", 4n)),
        ]}
      />,
    );

    const route = screen.getByRole("combobox", { name: "Inference route" });
    expect(route).toHaveValue("");
    expect(route).not.toBeRequired();
    expect(
      within(route)
        .getAllByRole("option")
        .map((option) => [option.textContent, (option as HTMLOptionElement).disabled]),
    ).toEqual([
      ["No inference route", false],
      ["retired — Disabled", false],
      ["primary — Available", false],
      ["idle — Unavailable (No eligible text member)", false],
    ]);
    expect(screen.queryByLabelText("Execution target")).not.toBeInTheDocument();
    expect(stepLabels()).toEqual(["1. Inference route", "2. Target", "3. Deploy"]);

    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByLabelText("Execution target")).toHaveValue("machine-a");
    await user.click(screen.getByRole("button", { name: "Next" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Deploy" })).toBeEnabled());
    await user.click(screen.getByRole("button", { name: "Deploy" }));

    expect(onSubmit).toHaveBeenCalledWith(
      { siteNodeId: "machine-a", executionTargetId: "machine-a" },
      {
        applicationDeploymentName: "OpenWebUI",
        inferenceRouteId: undefined,
        expectedInferenceRouteGeneration: 0n,
        parameterValues: [],
      },
    );
  });

  it.each([
    {
      expected:
        "This route is disabled. The application deploys with the link, but its inference requests fail until the route is enabled.",
      routeState: "disabled",
      view: disabledRoute(servingRoute("route-a", "primary", 6n)),
    },
    {
      expected:
        "No model serves this route right now. The application deploys with the link, but its inference requests fail until a route member is ready.",
      routeState: "member-less",
      view: unservedRoute(servingRoute("route-a", "primary", 6n)),
    },
  ])(
    "links a $routeState route and says its requests fail until it serves",
    async ({ expected, view }) => {
      const user = setupUser();
      const onSubmit = vi.fn();
      render(
        <ApplicationDeployDialog
          application={appWithOptionalRoute}
          onSubmit={onSubmit}
          onOpenChange={vi.fn()}
          open
          pending={false}
          executionTargets={[machine]}
          routes={[view]}
        />,
      );

      const route = screen.getByRole("combobox", { name: "Inference route" });
      await user.selectOptions(route, "route-a");

      expect(route).not.toHaveAttribute("aria-invalid");
      expect(route).toHaveAccessibleDescription(
        `Optional. A linked route passes its current name to the application. Without one, the application still reaches the inference API. ${expected}`,
      );
      await openDeploy(user);
      await waitFor(() => expect(screen.getByRole("button", { name: "Deploy" })).toBeEnabled());
      await user.click(screen.getByRole("button", { name: "Deploy" }));
      expect(onSubmit).toHaveBeenCalledWith(
        { siteNodeId: "local", executionTargetId: "local" },
        {
          applicationDeploymentName: "OpenWebUI",
          inferenceRouteId: "route-a",
          expectedInferenceRouteGeneration: 6n,
          parameterValues: [],
        },
      );
    },
  );

  it("offers chat and mixed routes but not decision-only routes", () => {
    render(
      <ApplicationDeployDialog
        application={appWithOptionalRoute}
        onSubmit={vi.fn()}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={[applicationTarget("machine-a")]}
        routes={[
          servingRoute("route-chat", "chat-only", 1n, [InferenceRequestKind.CHAT]),
          servingRoute("route-decision", "clef", 1n, [InferenceRequestKind.DECISION]),
          servingRoute("route-mixed", "mixed", 1n, [
            InferenceRequestKind.CHAT,
            InferenceRequestKind.DECISION,
          ]),
        ]}
      />,
    );

    expect(
      within(screen.getByRole("combobox", { name: "Inference route" }))
        .getAllByRole("option")
        .map((option) => option.textContent),
    ).toEqual(["No inference route", "chat-only — Available", "mixed — Available"]);
  });

  it("states no serving caveat for a serving route", async () => {
    const user = setupUser();
    render(
      <ApplicationDeployDialog
        application={appWithOptionalRoute}
        onSubmit={vi.fn()}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={[machine]}
        routes={[servingRoute("route-a", "primary", 3n)]}
      />,
    );

    const route = screen.getByRole("combobox", { name: "Inference route" });
    await user.selectOptions(route, "route-a");

    expect(route).toHaveAccessibleDescription(
      "Optional. A linked route passes its current name to the application. Without one, the application still reaches the inference API.",
    );
  });

  it("focuses an invalid default deployment name on Deploy without turning on Advanced", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    render(
      <ApplicationDeployDialog
        application={create(ApplicationSchema, { ...application, displayName: "   " })}
        executionTargets={[machine]}
        onOpenChange={vi.fn()}
        onSubmit={onSubmit}
        open
        pending={false}
      />,
    );

    expectDeployOnly();
    await waitFor(() => expect(screen.getByRole("button", { name: "Deploy" })).toBeEnabled());
    await user.click(screen.getByRole("button", { name: "Deploy" }));

    const nameControl = screen.getByRole("textbox", { name: "Deployment name" });
    expect(screen.getByRole("checkbox", { name: "Advanced" })).not.toBeChecked();
    expect(nameControl).toHaveFocus();
    expect(nameControl).toHaveAccessibleDescription("Deployment name is required.");
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it("keeps a linked route across polling and blocks deploy when its ID disappears", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    const alpha = applicationTarget("machine-a");
    const beta = applicationTarget("machine-b");
    const routeA = servingRoute("route-a", "primary", 3n);
    const routeB = servingRoute("route-b", "secondary", 4n);
    const view = render(
      <ApplicationDeployDialog
        application={appWithOptionalRoute}
        onSubmit={onSubmit}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={[beta, alpha]}
        routes={[routeA, routeB]}
      />,
    );
    const routeControl = screen.getByRole("combobox", { name: "Inference route" });
    await user.selectOptions(routeControl, "route-a");

    const refreshedRouteA = servingRoute("route-a", "primary-renamed", 5n);
    view.rerender(
      <ApplicationDeployDialog
        application={appWithOptionalRoute}
        onSubmit={onSubmit}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={[alpha, beta]}
        routes={[routeB, refreshedRouteA]}
      />,
    );

    expect(routeControl).toHaveValue("route-a");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByLabelText("Execution target")).toHaveValue("machine-a");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Step 3 of 3 — Deploy")).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    expect(onSubmit).toHaveBeenCalledWith(
      { siteNodeId: "machine-a", executionTargetId: "machine-a" },
      {
        applicationDeploymentName: "OpenWebUI",
        inferenceRouteId: "route-a",
        expectedInferenceRouteGeneration: 5n,
        parameterValues: [],
      },
    );

    view.rerender(
      <ApplicationDeployDialog
        application={appWithOptionalRoute}
        onSubmit={onSubmit}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={[alpha, beta]}
        routes={[routeB, unservedRoute(refreshedRouteA)]}
      />,
    );
    await user.click(screen.getByRole("button", { name: "Back" }));
    await user.click(screen.getByRole("button", { name: "Back" }));
    expect(screen.getByRole("combobox", { name: "Inference route" })).toHaveValue("route-a");
    expect(
      screen.getByRole("option", {
        name: "primary-renamed — Unavailable (No eligible text member)",
      }),
    ).toBeEnabled();
    expect(screen.getByRole("button", { name: "Next" })).toBeEnabled();

    view.rerender(
      <ApplicationDeployDialog
        application={appWithOptionalRoute}
        onSubmit={onSubmit}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={[beta]}
        routes={[routeB]}
      />,
    );

    const removedRouteControl = screen.getByRole("combobox", { name: "Inference route" });
    expect(removedRouteControl).toHaveValue("route-a");
    expect(screen.getByRole("option", { name: "Removed route" })).toBeDisabled();
    expect(removedRouteControl).toHaveAttribute("aria-invalid", "true");
    expect(removedRouteControl).toHaveAccessibleDescription(
      "Optional. A linked route passes its current name to the application. Without one, the application still reaches the inference API. The selected inference route no longer exists. Select another route or no route.",
    );
    await waitFor(() => expect(screen.getByRole("button", { name: "Next" })).toBeDisabled());

    await user.selectOptions(removedRouteControl, "");
    expect(screen.queryByRole("option", { name: "Removed route" })).not.toBeInTheDocument();
    expect(removedRouteControl).not.toHaveAttribute("aria-invalid");
    await openAdvanced(user);
    await waitFor(() => expect(screen.getByLabelText("Execution target")).toHaveValue(""));
  });

  it("waits for current execution-target data before applying the machine default", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    const staleMachine = applicationTarget("stale");
    const currentMachine = applicationTarget("current");
    const view = render(
      <ApplicationDeployDialog
        application={application}
        canCreate={false}
        executionTargets={[staleMachine]}
        executionTargetsCurrent={false}
        onOpenChange={vi.fn()}
        onSubmit={onSubmit}
        open
        pending={false}
      />,
    );

    view.rerender(
      <ApplicationDeployDialog
        application={application}
        executionTargets={[currentMachine]}
        executionTargetsCurrent
        onOpenChange={vi.fn()}
        onSubmit={onSubmit}
        open
        pending={false}
      />,
    );

    expectDeployOnly();
    await waitFor(() => expect(screen.getByRole("button", { name: "Deploy" })).toBeEnabled());
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    expect(onSubmit).toHaveBeenCalledWith(
      { siteNodeId: "current", executionTargetId: "current" },
      {
        applicationDeploymentName: "OpenWebUI",
        inferenceRouteId: undefined,
        expectedInferenceRouteGeneration: 0n,
        parameterValues: [],
      },
    );
  });
  it("revalidates the selected exact target immediately before application submission", async () => {
    const onSubmit = vi.fn();
    // The watched targets still show the target online when the fresh read finds it offline.
    const listTargets = vi
      .fn()
      .mockResolvedValue({ executionTargets: [applicationTarget("local", false)] });
    const user = setupUser();

    render(
      <ApplicationDeployDialog
        application={application}
        executionTargets={[]}
        listConfiguredExecutionTargets={listTargets}
        onOpenChange={vi.fn()}
        onSubmit={onSubmit}
        open
        pending={false}
      />,
      {
        watch: executionTargetsWatch(() =>
          Promise.resolve({ executionTargets: [applicationTarget("local")] }),
        ),
      },
    );
    expectDeployOnly();
    await waitFor(() => expect(screen.getByRole("button", { name: "Deploy" })).toBeEnabled());
    await user.click(screen.getByRole("button", { name: "Deploy" }));

    await waitFor(() => expect(listTargets).toHaveBeenCalledOnce());
    expect(onSubmit).not.toHaveBeenCalled();
    expect(screen.getByRole("alert", { name: "Deploy execution target status" })).toHaveTextContent(
      "Configured targets are visible, but none are deployable.",
    );
    expect(screen.getByRole("button", { name: "Deploy" })).toBeDisabled();
    await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
    await user.click(screen.getByRole("button", { name: "Back" }));
    expect(screen.getByText("Step 1 of 2 — Target")).toBeVisible();
    expect(screen.getByLabelText("Execution target")).toHaveValue("");
    expect(screen.getByRole("button", { name: "Next" })).toBeDisabled();
  });

  it("offers an eligible remote target with its node and platform beside the manager default", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    const local = create(ConfiguredExecutionTargetSchema, {
      displayName: "Docker applications",
      executionTargetId: "local-app",
      siteNodeId: "manager",
      platformLabel: "linux/amd64",
      acceleratorDeviceIds: [],
      workloadKind: ExecutionTargetWorkloadKind.APPLICATION,
      siteManager: true,
      availability: {
        connectionState: NodeObservationState.CONNECTED,
        runtimeAvailability: { availability: { case: "available", value: {} } },
      },
    });
    const remote = remoteApplicationTarget("remote-app", "alpha-worker");
    const staleRemote = remoteApplicationTarget(
      "stale-app",
      "beta-worker",
      NodeObservationState.STALE,
    );

    render(
      <ApplicationDeployDialog
        application={application}
        executionTargets={[remote, staleRemote, local]}
        onOpenChange={vi.fn()}
        onSubmit={onSubmit}
        open
        pending={false}
      />,
    );

    // Two deployable targets: basic mode opens on Target.
    expect(stepLabels()).toEqual(["1. Target", "2. Deploy"]);
    const targetControl = screen.getByLabelText("Execution target");
    // The remote node sorts first, but the site manager's target stays the default.
    await waitFor(() => expect(targetControl).toHaveValue("local-app"));
    expect(
      screen.getByRole("option", {
        name: "Name of alpha-worker / Docker applications — linux/arm64",
      }),
    ).toBeEnabled();
    expect(
      screen.getByRole("option", { name: "manager / Docker applications — linux/amd64" }),
    ).toBeEnabled();
    expect(
      screen.getByRole("option", {
        name: /^Name of beta-worker \/ Docker applications — Machine observation is overdue\. — linux\/arm64/,
      }),
    ).toBeDisabled();
    const section = screen.getByRole("heading", { level: 2, name: "Target" }).parentElement;
    expect(section).not.toBeNull();
    expect(detailTexts(section!)).toEqual([
      ["Machine", "manager"],
      ["Target", "Docker applications"],
      ["Target ID", "local-app"],
      ["Platform", "linux/amd64"],
      ["Configured devices", "None configured"],
      ["Workload kind", "Application"],
      ["Connection freshness", "Connected"],
      ["Runtime availability", "Available"],
    ]);

    await user.selectOptions(targetControl, "remote-app");
    expect(within(section!).getByText("Name of alpha-worker")).toBeVisible();
    expect(within(section!).getByText("remote-app")).toBeVisible();
    expect(within(section!).getByText("linux/arm64")).toBeVisible();
    expect(within(section!).getByText("Connected")).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    expect(onSubmit).toHaveBeenCalledWith(
      { siteNodeId: "alpha-worker", executionTargetId: "remote-app" },
      expect.objectContaining({ applicationDeploymentName: "OpenWebUI" }),
    );
  });

  it("names a node's targets by their display names, without their IDs", () => {
    render(
      <ApplicationDeployDialog
        application={application}
        executionTargets={[
          remoteApplicationTarget(
            "apps-a",
            "alpha-worker",
            NodeObservationState.CONNECTED,
            "Docker applications",
          ),
          remoteApplicationTarget(
            "apps-b",
            "alpha-worker",
            NodeObservationState.CONNECTED,
            "Simulated applications",
          ),
          remoteApplicationTarget("apps-c", "beta-worker"),
        ]}
        onOpenChange={vi.fn()}
        onSubmit={vi.fn()}
        open
        pending={false}
      />,
    );

    const options = within(screen.getByLabelText("Execution target")).getAllByRole("option");
    expect(options.map((option) => option.textContent)).toEqual([
      "Select an eligible execution target",
      "Name of alpha-worker / Docker applications — linux/arm64",
      "Name of alpha-worker / Simulated applications — linux/arm64",
      "Name of beta-worker / Docker applications — linux/arm64",
    ]);
  });

  it("defaults to a deployable remote target when the manager target is unavailable", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    render(
      <ApplicationDeployDialog
        application={application}
        executionTargets={[
          applicationTarget("manager", false),
          remoteApplicationTarget("remote-app"),
        ]}
        onOpenChange={vi.fn()}
        onSubmit={onSubmit}
        open
        pending={false}
      />,
    );

    expectDeployOnly();
    await waitFor(() => expect(screen.getByRole("button", { name: "Deploy" })).toBeEnabled());
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    expect(onSubmit).toHaveBeenCalledWith(
      { siteNodeId: "worker", executionTargetId: "remote-app" },
      expect.objectContaining({ applicationDeploymentName: "OpenWebUI" }),
    );
  });

  it("keeps the inference route unlinked by default", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    render(
      <ApplicationDeployDialog
        application={appWithOptionalRoute}
        onSubmit={onSubmit}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={[machine]}
        routes={[
          create(InferenceRouteViewSchema, {
            route: create(InferenceRouteSchema, {
              inferenceRouteId: "route-a",
              name: "Primary",
              enabled: true,
              generation: 3n,
            }),
            availability: create(InferenceRouteAvailabilitySchema, {
              available: true,
              availableInputModalities: [ModelInputModality.TEXT],
            }),
          }),
        ]}
      />,
    );

    expect(screen.getByRole("combobox", { name: "Inference route" })).toHaveValue("");
    await openDeploy(user);
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    expect(onSubmit).toHaveBeenCalledWith(
      { siteNodeId: "local", executionTargetId: "local" },
      {
        applicationDeploymentName: "OpenWebUI",
        inferenceRouteId: undefined,
        expectedInferenceRouteGeneration: 0n,
        parameterValues: [],
      },
    );
  });

  it("uses the shared deployment frame above its modal overlay", () => {
    render(
      <ApplicationDeployDialog
        application={appWithOptionalRoute}
        onSubmit={vi.fn()}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={[machine]}
      />,
    );

    const dialog = screen.getByRole("dialog", { name: "OpenWebUI — Deploy application" });
    const body = within(dialog).getByTestId("deployment-dialog-body");
    const footer = within(dialog).getByTestId("deployment-dialog-footer");
    const start = within(footer).getByTestId("deployment-dialog-footer-start");
    const actions = within(footer).getByTestId("deployment-dialog-actions");
    expectResourceMarker(inspectorTitle(dialog), "open-webui");
    expect(within(dialog).getByText("OpenWebUI")).toHaveClass("uppercase");
    expect(within(dialog).getByText("Deploy application")).toHaveClass("text-xl");
    expect(dialog).toHaveClass(
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
    expect(body).toHaveClass("min-h-0", "overflow-y-auto", "sm:px-6");
    expect(footer).toHaveClass("justify-between", "sm:px-6");
    expect(start.compareDocumentPosition(actions) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(within(start).getByRole("checkbox", { name: "Advanced" })).not.toBeChecked();
    expect(
      within(actions)
        .getAllByRole("button")
        .map((button) => button.textContent),
    ).toEqual(["Cancel", "Next"]);
    expect(dialog).toHaveStyle({ zIndex: "calc(var(--layer-modal) + 1)" });
  });

  it("orders the advanced steps with Target before Deploy", async () => {
    const user = setupUser();
    render(
      <ApplicationDeployDialog
        application={appWithOptionalRoute}
        onSubmit={vi.fn()}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={[machine]}
      />,
    );

    const dialog = screen.getByRole("dialog", { name: "OpenWebUI — Deploy application" });
    await openAdvanced(user, within(dialog));
    expect(stepLabels()).toEqual(["1. Inference route", "2. Target", "3. Deploy"]);
    expect(within(dialog).getByText("Step 2 of 3 — Target")).toBeVisible();
    expect(within(dialog).getByLabelText("Execution target")).toBeVisible();
    expect(
      within(dialog).queryByRole("textbox", { name: "Deployment name" }),
    ).not.toBeInTheDocument();

    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    expect(within(dialog).getByText("Step 3 of 3 — Deploy")).toBeVisible();
    expect(within(dialog).getByRole("textbox", { name: "Deployment name" })).toBeVisible();
    expect(within(dialog).queryByLabelText("Execution target")).not.toBeInTheDocument();
    expect(summaryRows()).toEqual([
      ["Inference route", "None"],
      ["Target", "local / Docker applications"],
      ["GPU devices", "none"],
    ]);
  });

  it("associates target and name errors with their controls", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    render(
      <ApplicationDeployDialog
        application={appWithOptionalRoute}
        onSubmit={onSubmit}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={[machine]}
      />,
    );

    await openAdvanced(user);
    const machineControl = screen.getByLabelText("Execution target");
    expect(machineControl).toBeRequired();
    await user.selectOptions(machineControl, "");
    await user.click(screen.getByRole("button", { name: "Next" }));

    expect(screen.getByText("Step 2 of 3 — Target")).toBeVisible();
    expect(machineControl).toHaveFocus();
    expect(machineControl).toHaveAttribute("aria-invalid", "true");
    expect(machineControl).toHaveAccessibleDescription("Select an eligible execution target.");
    await user.selectOptions(machineControl, "local");
    expect(machineControl).not.toHaveAttribute("aria-invalid");

    await user.click(screen.getByRole("button", { name: "Next" }));
    const nameControl = screen.getByRole("textbox", { name: "Deployment name" });
    await user.clear(nameControl);
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    expect(nameControl).toHaveFocus();
    expect(nameControl).toHaveAttribute("aria-invalid", "true");
    expect(nameControl).toHaveAccessibleDescription("Deployment name is required.");
    await user.type(nameControl, "Team chat");
    expect(nameControl).not.toHaveAttribute("aria-invalid");
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it("flags the deployment name on blur only after it changes", async () => {
    const user = setupUser();
    render(
      <ApplicationDeployDialog
        application={appWithOptionalRoute}
        executionTargets={[machine]}
        onOpenChange={vi.fn()}
        onSubmit={vi.fn()}
        open
        pending={false}
      />,
    );

    await openAdvanced(user);
    await user.click(screen.getByRole("button", { name: "Next" }));
    const nameControl = screen.getByRole("textbox", { name: "Deployment name" });
    await user.click(nameControl);
    await user.tab();
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 10));
    });
    expect(nameControl).not.toHaveAttribute("aria-invalid");

    await user.type(nameControl, "x");
    await user.clear(nameControl);
    await user.tab();
    await waitFor(() => expect(nameControl).toHaveAttribute("aria-invalid", "true"));
    expect(nameControl).toHaveAccessibleDescription("Deployment name is required.");
  });

  it("flags a cleared execution target on blur", async () => {
    const user = setupUser();
    render(
      <ApplicationDeployDialog
        application={appWithOptionalRoute}
        executionTargets={[machine]}
        onOpenChange={vi.fn()}
        onSubmit={vi.fn()}
        open
        pending={false}
      />,
    );

    await openAdvanced(user);
    const machineControl = screen.getByLabelText("Execution target");
    await user.click(machineControl);
    await user.tab();
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 10));
    });
    expect(machineControl).not.toHaveAttribute("aria-invalid");

    await user.selectOptions(machineControl, "");
    await user.tab();
    await waitFor(() => expect(machineControl).toHaveAttribute("aria-invalid", "true"));
    expect(machineControl).toHaveAccessibleDescription("Select an eligible execution target.");
  });

  it("opens Target and turns on Advanced when Deploy finds no eligible target", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    const view = render(
      <ApplicationDeployDialog
        application={application}
        onSubmit={onSubmit}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={[machine]}
      />,
    );

    expectDeployOnly();
    view.rerender(
      <ApplicationDeployDialog
        application={application}
        onSubmit={onSubmit}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={[applicationTarget("local", false), applicationTarget("spare")]}
      />,
    );
    expect(
      await screen.findByRole("alert", { name: "Deploy execution target status" }),
    ).toHaveTextContent(
      "The selected execution target is no longer eligible. Turn on Advanced to select an eligible execution target.",
    );
    await waitFor(() => expect(summaryRows()).toEqual([["Target", "Unavailable"]]));
    await user.click(screen.getByRole("button", { name: "Deploy" }));

    expect(screen.getByRole("checkbox", { name: "Advanced" })).toBeChecked();
    expect(screen.getByText("Step 1 of 2 — Target")).toBeVisible();
    const machineControl = screen.getByLabelText("Execution target");
    expect(machineControl).toHaveFocus();
    expect(machineControl).toHaveAccessibleDescription("Select an eligible execution target.");
    expect(screen.getByText("Advanced on to fix target settings")).toBeInTheDocument();
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it("keeps the steps fixed after the first step while the deployable targets change", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    const dialog = (executionTargets: ReturnType<typeof applicationTarget>[]) => (
      <ApplicationDeployDialog
        application={appWithOptionalRoute}
        onSubmit={onSubmit}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={executionTargets}
      />
    );
    const view = render(dialog([machine, applicationTarget("spare")]));

    expect(stepLabels()).toEqual(["1. Inference route", "2. Target", "3. Deploy"]);
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByLabelText("Execution target")).toHaveValue("local");
    await user.click(screen.getByRole("button", { name: "Next" }));
    view.rerender(dialog([applicationTarget("local", false), applicationTarget("spare")]));

    expect(
      await screen.findByRole("alert", { name: "Deploy execution target status" }),
    ).toHaveTextContent(
      "The selected execution target is no longer eligible. Return to Target and select an eligible execution target.",
    );
    expect(stepLabels()).toEqual(["1. Inference route", "2. Target", "3. Deploy"]);
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    expect(screen.getByText("Step 2 of 3 — Target")).toBeVisible();
    expect(screen.getByRole("checkbox", { name: "Advanced" })).not.toBeChecked();
    expect(onSubmit).not.toHaveBeenCalled();

    await user.click(screen.getByRole("button", { name: "Back" }));
    expect(stepLabels()).toEqual(["1. Inference route", "2. Deploy"]);
  });

  it("summarizes resolved values and keeps advanced values when Back returns to the first step", async () => {
    const user = setupUser();
    const route = servingRoute("route-a", "Primary", 7n);
    const remoteMachine = applicationTarget("remote");
    render(
      <ApplicationDeployDialog
        application={appWithOptionalRoute}
        onSubmit={vi.fn()}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={[remoteMachine, machine]}
        routes={[route]}
      />,
    );
    await user.selectOptions(screen.getByRole("combobox", { name: "Inference route" }), "route-a");

    await openAdvanced(user);
    await user.selectOptions(screen.getByLabelText("Execution target"), "remote");
    await user.click(screen.getByRole("button", { name: "Next" }));
    const nameControl = screen.getByLabelText("Deployment name");
    await user.clear(nameControl);
    await user.type(nameControl, "Team chat");

    const summary = screen.getByRole("region", { name: "Summary" });
    expect(summaryRows()).toEqual([
      ["Inference route", "PrimaryAvailable"],
      ["Target", "remote / Docker applications"],
      ["GPU devices", "none"],
    ]);
    expect(within(summary).getByText("Available")).toBeVisible();
    expect(within(summary).queryByText("7")).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Back" }));
    expect(screen.getByLabelText("Execution target")).toHaveValue("remote");
    await user.click(screen.getByRole("button", { name: "Back" }));
    expect(screen.getByText("Step 1 of 3 — Inference route")).toBeVisible();
    expect(screen.getByRole("combobox", { name: "Inference route" })).toHaveValue("route-a");

    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByLabelText("Execution target")).toHaveValue("remote");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByLabelText("Deployment name")).toHaveValue("Team chat");
  });

  it("navigates visited steps from their headers and keeps values at the first step", async () => {
    const user = setupUser();
    const remoteMachine = applicationTarget("remote");
    render(
      <ApplicationDeployDialog
        application={appWithOptionalRoute}
        onSubmit={vi.fn()}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={[remoteMachine, machine]}
        routes={[servingRoute("route-a", "Primary", 7n)]}
      />,
    );

    await openAdvanced(user);
    const progress = screen.getByRole("list", { name: "Deployment progress" });
    expect(within(progress).getByRole("button", { name: "3. Deploy" })).toBeDisabled();
    await user.selectOptions(screen.getByLabelText("Execution target"), "remote");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Step 3 of 3 — Deploy")).toBeVisible();
    expect(screen.getByRole("button", { name: "Back" })).not.toHaveAttribute("aria-describedby");

    await user.click(within(progress).getByRole("button", { name: "2. Target" }));
    expect(screen.getByText("Step 2 of 3 — Target")).toBeVisible();
    expect(screen.getByLabelText("Execution target")).toHaveValue("remote");
    await user.click(within(progress).getByRole("button", { name: "3. Deploy" }));
    expect(screen.getByText("Step 3 of 3 — Deploy")).toBeVisible();

    await user.click(within(progress).getByRole("button", { name: "1. Inference route" }));
    expect(screen.getByText("Step 1 of 3 — Inference route")).toBeVisible();
    await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
    expect(stepLabels()).toEqual(["1. Inference route", "2. Target", "3. Deploy"]);
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByLabelText("Execution target")).toHaveValue("remote");
  });

  it("summarizes an unlinked route input as None", async () => {
    const user = setupUser();
    render(
      <ApplicationDeployDialog
        application={appWithOptionalRoute}
        onSubmit={vi.fn()}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={[machine]}
        routes={[servingRoute("route-a", "Primary", 7n)]}
      />,
    );

    await openDeploy(user);
    expect(summaryRows()).toEqual([
      ["Inference route", "None"],
      ["Target", "local / Docker applications"],
      ["GPU devices", "none"],
    ]);
  });

  it("surfaces execution-target and route invalidation while Deploy is open", async () => {
    const user = setupUser();
    const route = servingRoute("route-a", "Primary", 7n);
    const view = render(
      <ApplicationDeployDialog
        application={appWithOptionalRoute}
        executionTargets={[machine]}
        onOpenChange={vi.fn()}
        onSubmit={vi.fn()}
        open
        pending={false}
        routes={[route]}
      />,
    );
    await user.selectOptions(screen.getByRole("combobox", { name: "Inference route" }), "route-a");
    await openAdvanced(user);
    await user.click(screen.getByRole("button", { name: "Next" }));

    view.rerender(
      <ApplicationDeployDialog
        application={appWithOptionalRoute}
        executionTargets={[applicationTarget("local", false), applicationTarget("spare")]}
        onOpenChange={vi.fn()}
        onSubmit={vi.fn()}
        open
        pending={false}
        routes={[unservedRoute(route)]}
      />,
    );

    expect(
      await screen.findByRole("alert", { name: "Deploy execution target status" }),
    ).toHaveTextContent(
      "The selected execution target is no longer eligible. Return to Target and select an eligible execution target.",
    );
    expect(screen.queryByRole("alert", { name: "Deploy route status" })).not.toBeInTheDocument();
    await waitFor(() =>
      expect(summaryRows()).toEqual([
        ["Inference route", "PrimaryUnavailable"],
        ["Target", "Unavailable"],
      ]),
    );

    view.rerender(
      <ApplicationDeployDialog
        application={appWithOptionalRoute}
        executionTargets={[]}
        onOpenChange={vi.fn()}
        onSubmit={vi.fn()}
        open
        pending={false}
        routes={[]}
      />,
    );

    expect(screen.getByRole("alert", { name: "Deploy execution target status" })).toHaveTextContent(
      "No application execution targets are configured.",
    );
    expect(screen.getByRole("alert", { name: "Deploy route status" })).toHaveTextContent(
      "The selected inference route no longer exists. Select another route or no route. Return to Inference route to change the link.",
    );
    expect(summaryRows()[0]).toEqual(["Inference route", "Removed route"]);
    expect(screen.getByRole("button", { name: "Deploy" })).toBeDisabled();
  });

  it("offers only the unlinked choice when no route exists", async () => {
    render(
      <ApplicationDeployDialog
        application={appWithOptionalRoute}
        onSubmit={vi.fn()}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={[machine]}
      />,
    );
    const routeControl = screen.getByRole("combobox", { name: "Inference route" });

    expect(routeControl).not.toBeRequired();
    expect(routeControl).toHaveAttribute("data-slot", "native-select");
    expect(
      within(routeControl)
        .getAllByRole("option")
        .map((option) => option.textContent),
    ).toEqual(["No inference route"]);
    await waitFor(() => expect(screen.getByRole("button", { name: "Next" })).toBeEnabled());
    expect(screen.getByRole("checkbox", { name: "Advanced" })).toBeEnabled();
  });

  it("deploys a route-less application without inventing route input", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    render(
      <ApplicationDeployDialog
        application={application}
        onSubmit={onSubmit}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={[machine]}
      />,
    );

    expect(screen.queryByRole("combobox")).not.toBeInTheDocument();
    expectDeployOnly();
    expect(screen.getByRole("textbox", { name: "Deployment name" })).toHaveValue("OpenWebUI");
    await waitFor(() =>
      expect(summaryRows()).toEqual([
        ["Target", "local / Docker applications"],
        ["GPU devices", "none"],
      ]),
    );
    expect(screen.queryByText(/Turn on Advanced to choose/)).not.toBeInTheDocument();
    expect(summaryRows()).toEqual([
      ["Target", "local / Docker applications"],
      ["GPU devices", "none"],
    ]);
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    expect(onSubmit).toHaveBeenCalledWith(
      { siteNodeId: "local", executionTargetId: "local" },
      {
        applicationDeploymentName: "OpenWebUI",
        inferenceRouteId: undefined,
        expectedInferenceRouteGeneration: 0n,
        parameterValues: [],
      },
    );
  });

  it("submits once when Enter is pressed in the deployment name", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    render(
      <ApplicationDeployDialog
        application={application}
        onSubmit={onSubmit}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={[machine]}
      />,
    );

    expectDeployOnly();
    await user.click(screen.getByRole("textbox", { name: "Deployment name" }));
    await user.keyboard("{Enter}");

    expect(onSubmit).toHaveBeenCalledOnce();
  });

  it("blocks dismissal and duplicate submission while pending", async () => {
    const user = setupUser();
    const onOpenChange = vi.fn();
    const onSubmit = vi.fn();
    const view = render(
      <ApplicationDeployDialog
        application={application}
        onSubmit={onSubmit}
        onOpenChange={onOpenChange}
        open
        pending={false}
        executionTargets={[machine]}
      />,
    );
    expectDeployOnly();
    view.rerender(
      <ApplicationDeployDialog
        application={application}
        onSubmit={onSubmit}
        onOpenChange={onOpenChange}
        open
        pending
        executionTargets={[machine]}
      />,
    );
    expect(screen.getByRole("button", { name: "Deploying…" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Cancel" })).toBeDisabled();
    expect(screen.queryByRole("button", { name: "Close" })).not.toBeInTheDocument();
    await user.keyboard("{Escape}");
    expect(onOpenChange).not.toHaveBeenCalled();
  });
});

describe("Application deployment dialog GPU devices", () => {
  function renderDialog(target: ReturnType<typeof applicationTarget>) {
    render(
      <ApplicationDeployDialog
        application={application}
        onSubmit={vi.fn()}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={[target]}
      />,
    );
  }

  // The default target applies once the dialog has rendered.
  async function summaryRows() {
    await act(async () => {});
    return detailTexts(screen.getByRole("dialog"));
  }

  it("lists the devices the selected target passes on the Deploy step", async () => {
    const target = applicationTarget("local");
    target.acceleratorDeviceIds = ["nvidia.com/gpu=0", "nvidia.com/gpu=1"];
    renderDialog(target);

    expect(await summaryRows()).toEqual([
      ["Target", "local / Docker applications"],
      ["GPU devices", "nvidia.com/gpu=0, nvidia.com/gpu=1"],
    ]);
  });

  it("says none when the selected target passes no devices", async () => {
    renderDialog(applicationTarget("local"));

    expect(await summaryRows()).toEqual([
      ["Target", "local / Docker applications"],
      ["GPU devices", "none"],
    ]);
  });

  it("shows no fit wording or estimate read", async () => {
    renderDialog(applicationTarget("local"));
    await summaryRows();

    expect(screen.queryByText(/memory fit|estimat/i)).toBeNull();
  });
});

/** Each label/value pair of the detail lists within `element`, in order. */
function detailTexts(element: HTMLElement): string[][] {
  return [...element.querySelectorAll("dt")].map((label) => [
    label.textContent ?? "",
    label.nextElementSibling?.textContent ?? "",
  ]);
}

describe("Application deployment dialog parameters", () => {
  const machine = applicationTarget("local");
  const parameterized = create(ApplicationSchema, {
    applicationId: "comfyui",
    displayName: "ComfyUI",
    version: "1.0.0",
    graph: create(ApplicationGraphSchema, {
      parameters: [
        create(ParameterDefinitionSchema, {
          parameterId: "vram-mode",
          displayName: "VRAM mode",
          description: "How models stay in GPU memory.",
          visibility: ParameterVisibility.ADVANCED,
          parameterType: {
            case: "stringEnum",
            value: { values: ["auto", "highvram"], defaultValue: "auto" },
          },
        }),
        create(ParameterDefinitionSchema, {
          parameterId: "workers",
          displayName: "Workers",
          visibility: ParameterVisibility.BASIC,
          parameterType: {
            case: "unsignedInteger",
            value: { minimum: 1n, maximum: 8n, unit: "", defaultValue: 2n },
          },
        }),
      ],
    }),
  });

  // ComfyUI's catalog shape: one advanced parameter and no basic one.
  const advancedOnly = create(ApplicationSchema, {
    ...parameterized,
    graph: create(ApplicationGraphSchema, { parameters: [parameterized.graph!.parameters[0]] }),
  });

  function renderDialog(onSubmit = vi.fn(), app = parameterized) {
    render(
      <ApplicationDeployDialog
        application={app}
        onSubmit={onSubmit}
        onOpenChange={vi.fn()}
        open
        pending={false}
        executionTargets={[machine]}
      />,
    );
    return onSubmit;
  }

  it("shows basic parameters on Settings, advanced ones on Advanced, and submits only changed values", async () => {
    const user = setupUser();
    const onSubmit = renderDialog();

    expect(stepLabels()).toEqual(["1. Settings", "2. Deploy"]);
    expect(screen.getByRole("heading", { level: 2, name: "Settings" })).toBeInTheDocument();
    const workers = screen.getByRole("textbox", { name: "Workers" });
    expect(workers).toHaveValue("2");
    await user.click(screen.getByRole("button", { name: "About Workers" }));
    expect(await screen.findByRole("dialog", { name: "About Workers" })).toHaveTextContent(
      "Range 1–8",
    );
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("combobox", { name: "VRAM mode" })).toBeNull();
    await user.clear(workers);
    await user.type(workers, "4");

    await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
    expect(stepLabels()).toEqual(["1. Target", "2. Settings", "3. Advanced", "4. Deploy"]);
    expect(screen.getByText("Step 2 of 4 — Settings")).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Step 3 of 4 — Advanced")).toBeVisible();
    expect(screen.queryByRole("textbox", { name: "Workers" })).toBeNull();
    const vramMode = screen.getByRole("combobox", { name: "VRAM mode" });
    expect(vramMode).toHaveValue("auto");
    expect(
      within(vramMode)
        .getAllByRole("option")
        .map((option) => option.textContent),
    ).toEqual(["auto", "highvram"]);
    await user.selectOptions(vramMode, "highvram");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(summaryRows()).toEqual([
      ["Target", "local / Docker applications"],
      ["GPU devices", "none"],
      ["VRAM mode", "highvram"],
      ["Workers", "4"],
    ]);
    await user.click(screen.getByRole("button", { name: "Deploy" }));

    expect(onSubmit).toHaveBeenCalledWith(
      { siteNodeId: "local", executionTargetId: "local" },
      {
        applicationDeploymentName: "ComfyUI",
        inferenceRouteId: undefined,
        expectedInferenceRouteGeneration: 0n,
        parameterValues: [
          create(ParameterValueSchema, {
            parameterId: "vram-mode",
            value: { case: "string", value: "highvram" },
          }),
          create(ParameterValueSchema, {
            parameterId: "workers",
            value: { case: "unsignedInteger", value: 4n },
          }),
        ],
      },
    );
  });

  it("opens an application with only advanced parameters on Deploy, and on Target in Advanced", async () => {
    const user = setupUser();
    renderDialog(vi.fn(), advancedOnly);

    expectDeployOnly();
    expect(screen.queryByRole("combobox", { name: "VRAM mode" })).toBeNull();

    await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
    expect(stepLabels()).toEqual(["1. Target", "2. Advanced", "3. Deploy"]);
    expect(screen.getByText("Step 3 of 3 — Deploy")).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Back" }));
    expect(screen.getByText("Step 2 of 3 — Advanced")).toBeVisible();
    expect(screen.getByRole("combobox", { name: "VRAM mode" })).toHaveValue("auto");

    // Turning Advanced off removes the step under the operator, who falls forward to Deploy.
    await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
    expectDeployOnly();
  });

  it("turns Advanced on and focuses an invalid advanced parameter when Deploy finds it", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    const withoutDefault = create(ApplicationSchema, {
      ...parameterized,
      graph: create(ApplicationGraphSchema, {
        parameters: [
          create(ParameterDefinitionSchema, {
            parameterId: "cache-size",
            displayName: "Cache size",
            visibility: ParameterVisibility.ADVANCED,
            parameterType: {
              case: "unsignedInteger",
              value: { minimum: 1n, maximum: 64n, unit: "GiB" },
            },
          }),
        ],
      }),
    });
    renderDialog(onSubmit, withoutDefault);

    expectDeployOnly();
    await waitFor(() =>
      expect(summaryRows()[0]).toEqual(["Target", "local / Docker applications"]),
    );
    await user.click(screen.getByRole("button", { name: "Deploy" }));

    expect(screen.getByText("Step 2 of 3 — Advanced")).toBeVisible();
    expect(screen.getByRole("checkbox", { name: "Advanced" })).toBeChecked();
    expect(screen.getByText("Advanced on to fix advanced settings")).toBeInTheDocument();
    const cacheSize = screen.getByRole("textbox", { name: "Cache size" });
    await waitFor(() => expect(cacheSize).toHaveFocus());
    expect(cacheSize).toHaveAttribute("aria-invalid", "true");
    expect(screen.getByText("Cache size must be an integer from 1 to 64 GiB.")).toBeVisible();
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it("blocks Next on an out-of-range value and focuses its field", async () => {
    const user = setupUser();
    renderDialog();
    // The dialog moves focus to its current step a frame after it opens.
    await waitFor(() => expect(screen.getByRole("button", { current: "step" })).toHaveFocus());

    const workers = screen.getByRole("textbox", { name: "Workers" });
    await user.clear(workers);
    await user.type(workers, "9");
    await user.click(screen.getByRole("button", { name: "Next" }));

    expect(screen.getByText("Workers must be an integer from 1 to 8.")).toBeVisible();
    expect(workers).toHaveFocus();
    expect(workers).toHaveAttribute("aria-invalid", "true");
    expect(screen.getByText("Step 1 of 2 — Settings")).toBeVisible();
  });

  describe("text and secret parameters", () => {
    const login = create(ApplicationSchema, {
      applicationId: "hermes",
      displayName: "Hermes",
      version: "1.0.0",
      graph: create(ApplicationGraphSchema, {
        parameters: [
          create(ParameterDefinitionSchema, {
            parameterId: "dashboard-username",
            displayName: "Username",
            visibility: ParameterVisibility.BASIC,
            parameterType: {
              case: "text",
              value: { minimumLength: 1, maximumLength: 8, defaultValue: "admin" },
            },
          }),
          create(ParameterDefinitionSchema, {
            parameterId: "dashboard-password",
            displayName: "Password",
            visibility: ParameterVisibility.BASIC,
            parameterType: { case: "secret", value: { generatedLength: 32, minimumLength: 12 } },
          }),
        ],
      }),
    });

    it("leaves an empty secret out of the request and sends a typed one", async () => {
      const user = setupUser();
      const onSubmit = renderDialog(vi.fn(), login);

      expect(screen.getByRole("textbox", { name: "Username" })).toHaveValue("admin");
      const password = screen.getByRole("textbox", { name: "Password" });
      expect(password).toHaveValue("");
      expect(password).toHaveAttribute("placeholder", "Generated at deploy");
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Deploy" }));
      expect(onSubmit).toHaveBeenCalledWith(
        expect.anything(),
        expect.objectContaining({ parameterValues: [] }),
      );
    });

    it("sends a typed secret as typed", async () => {
      const user = setupUser();
      const onSubmit = renderDialog(vi.fn(), login);

      await user.type(screen.getByRole("textbox", { name: "Password" }), "typed-password-1");
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Deploy" }));
      expect(onSubmit).toHaveBeenCalledWith(
        expect.anything(),
        expect.objectContaining({
          parameterValues: [
            create(ParameterValueSchema, {
              parameterId: "dashboard-password",
              value: { case: "string", value: "typed-password-1" },
            }),
          ],
        }),
      );
    });

    it("shows a length or character error per field", async () => {
      const user = setupUser();
      renderDialog(vi.fn(), login);

      await user.type(screen.getByRole("textbox", { name: "Password" }), "short");
      await user.type(screen.getByRole("textbox", { name: "Username" }), "-way-too-long");
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Username must be 1 to 8 characters.")).toBeVisible();
      expect(screen.getByText("Password must be at least 12 characters.")).toBeVisible();

      const password = screen.getByRole("textbox", { name: "Password" });
      await user.clear(password);
      await user.type(password, "has=equals-sign");
      await user.tab();
      expect(screen.getByText(`Password can't contain "=".`)).toBeVisible();
    });
  });
});
