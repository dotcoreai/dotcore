import { create } from "@bufbuild/protobuf";
import { timestampFromDate } from "@bufbuild/protobuf/wkt";
import { render, screen, waitFor, within } from "@testing-library/react";
import { createRef } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { DeploymentLogOperations } from "@/api/deployment-log-client";
import {
  ApplicationDeploymentDesiredState,
  ApplicationDeploymentSchema,
  type ApplicationServiceAcceleratorMemory,
  ApplicationServiceAcceleratorMemorySchema,
} from "@/gen/dotcore/application/v1/deployment_pb";
import {} from "@/gen/dotcore/application/v1/service_pb";
import {
  ApplicationDeploymentStateSchema,
  ApplicationDeploymentStep,
  ApplicationLifecycleState,
  ApplicationReadinessState,
} from "@/gen/dotcore/application/v1/state_pb";
import type { Application } from "@/gen/dotcore/catalog/v1/application_pb";
import {
  ApplicationConfigurationBindingSchema,
  ApplicationDeploymentInputSchema,
  ApplicationGraphSchema,
  ApplicationSchema,
  ApplicationServiceSchema,
} from "@/gen/dotcore/catalog/v1/application_pb";
import { ParameterVisibility } from "@/gen/dotcore/catalog/v1/parameter_pb";
import {
  type AcceleratorMemoryObservation,
  AcceleratorMemoryObservationSchema,
} from "@/gen/dotcore/deployment/v1/accelerator_memory_pb";
import {
  ConfiguredExecutionTargetSchema,
  ExecutionTargetWorkloadKind,
  MachineSchema,
  NodeObservationState,
} from "@/gen/dotcore/site/v1/service_pb";
import { chooseMoreAction, inspectorActionList } from "@/test/inspector-actions";
import { expectResourceMarker, inspectorTitle } from "@/test/resource-marker";
import { setupUser } from "@/test/user";

import { DeploymentApplicationInspector } from "./application-inspector";

const application = create(ApplicationSchema, {
  applicationId: "open-webui",
  displayName: "OpenWebUI",
  version: "0.6.0",
});

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
) {
  return create(ConfiguredExecutionTargetSchema, {
    displayName: "Docker applications",
    executionTargetId: id,
    siteNodeId,
    siteNodeDisplayName: "gb10-host-2",
    platformLabel: "linux/arm64",
    workloadKind: ExecutionTargetWorkloadKind.APPLICATION,
    availability: {
      connectionState,
      runtimeAvailability: { availability: { case: "available", value: {} } },
    },
  });
}

function resolvedSnapshot(application: Application) {
  return {
    applicationId: application.applicationId,
    displayName: application.displayName,
    version: application.version,
    notes: application.notes,
    graph: application.graph,
    placement: { siteNodeId: "local", executionTargetId: "local" },
  };
}

/** Each label/value pair of the detail lists within `element`, in order. */
function detailTexts(element: HTMLElement): string[][] {
  return [...element.querySelectorAll("dt")].map((label) => [
    label.textContent ?? "",
    label.nextElementSibling?.textContent ?? "",
  ]);
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
function sectionNames(): string[] {
  return [...sectionWrapper().querySelectorAll(":scope > section")].map(
    (section) =>
      document.getElementById(section.getAttribute("aria-labelledby") ?? "")?.textContent ?? "",
  );
}

/** Tag names of any inspector body child that is not a section, which the section system forbids. */
function nonSectionChildren(): string[] {
  return [...sectionWrapper().children]
    .filter((child) => child.tagName !== "SECTION")
    .map((child) => child.tagName);
}

function sectionWrapper(): HTMLElement {
  const wrapper = document.querySelector(".inspector-sections");
  if (!wrapper) {
    throw new Error("no inspector section wrapper rendered");
  }

  return wrapper as HTMLElement;
}

describe("Application deployment dialog", () => {
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

  it("marks the deployment inspector title with the deployment's own color", () => {
    render(
      <DeploymentApplicationInspector
        deployment={create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "deployment-1",
          applicationDeploymentName: "Team chat",
          snapshot: resolvedSnapshot(application),
        })}
        onRemove={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );
    expectResourceMarker(
      inspectorTitle(screen.getByRole("complementary", { name: "Team chat" })),
      "deployment-1",
    );
  });

  it("focuses the deployment inspector heading when it opens", async () => {
    render(
      <DeploymentApplicationInspector
        deployment={create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "deployment-1",
          applicationDeploymentName: "Team chat",
          snapshot: resolvedSnapshot(application),
        })}
        onRemove={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    const panel = screen.getByRole("complementary", { name: "Team chat" });
    await waitFor(() =>
      expect(within(panel).getByRole("heading", { name: "Team chat" })).toHaveFocus(),
    );
  });

  it("shows controller state and opens the exact assigned POC URL before readiness", () => {
    const endpointUrl = "http://100.64.0.7:49152";
    const deployment = create(ApplicationDeploymentSchema, {
      applicationDeploymentId: "deployment-1",
      applicationDeploymentName: "open-webui-1",
      snapshot: resolvedSnapshot(application),
      state: create(ApplicationDeploymentStateSchema, {
        endpoint: { url: endpointUrl },
        lifecycleState: ApplicationLifecycleState.RUNNING,
        readinessState: ApplicationReadinessState.NOT_READY,
      }),
      placement: { siteNodeId: "local", executionTargetId: "local" },
    });

    render(
      <DeploymentApplicationInspector
        deployment={deployment}
        onRemove={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    expect(screen.getAllByText("Not ready")[0]).toBeVisible();
    const access = screen.getByRole("region", { name: "Access" });
    expect(within(access).queryByRole("link")).not.toBeInTheDocument();
    const link = within(screen.getByTestId("inspector-footer")).getByRole("link", { name: "Open" });
    expect(link).toHaveClass("bg-primary");
    expect(link).toHaveAttribute("href", endpointUrl);
    expect(link).toHaveAttribute("target", "_blank");
    expect(within(access).getByText(endpointUrl)).toBeVisible();
  });

  it("states the selected route once, as a marked name in the Deployment section", () => {
    const deployment = create(ApplicationDeploymentSchema, {
      applicationDeploymentId: "deployment-1",
      applicationDeploymentName: "open-webui-1",
      snapshot: resolvedSnapshot(application),
      selectedInferenceRoute: { inferenceRouteId: "route-1", routeName: "private-chat" },
    });

    render(
      <DeploymentApplicationInspector
        deployment={deployment}
        onRemove={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    const section = screen.getByRole("region", { name: "Deployment" });
    expect(within(section).getByText("Inference route")).toBeVisible();
    expect(section).toHaveTextContent("private-chat");
    expect(section).not.toHaveTextContent("route-1");
    expectResourceMarker(within(section).getByText("private-chat"), "route-1");
  });

  it("links the placement machine, route, and application to their inspectors", () => {
    const machineHref =
      "/machines?panel=apps%2Fdeployment%3Adeployment-1&panel=machines%2Fmachine%3Aworker";
    const routeHref =
      "/routes?panel=apps%2Fdeployment%3Adeployment-1&panel=routes%2Froute%3Aroute-1";
    const applicationHref =
      "/apps/catalog?panel=apps%2Fdeployment%3Adeployment-1&panel=app-catalog%2Fcatalog%3Aopen-webui";
    render(
      <DeploymentApplicationInspector
        applicationHref={applicationHref}
        deployment={create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "deployment-1",
          applicationDeploymentName: "Team chat",
          placement: { siteNodeId: "worker", executionTargetId: "remote-app" },
          selectedInferenceRoute: { inferenceRouteId: "route-1", routeName: "private-chat" },
          snapshot: resolvedSnapshot(application),
        })}
        inferenceRouteHref={routeHref}
        onRemove={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
        placementMachine={create(MachineSchema, {
          displayName: "gb10-host-2",
          machineId: "worker",
        })}
        placementMachineHref={machineHref}
        executionTargets={[remoteApplicationTarget("remote-app")]}
      />,
    );

    const deploymentSection = screen.getByRole("region", { name: "Deployment" });
    const machine = within(deploymentSection).getByRole("link", {
      name: "gb10-host-2 / Docker applications",
    });
    expect(machine).toHaveAttribute("href", machineHref);
    expectResourceMarker(machine, "worker");
    const route = within(deploymentSection).getByRole("link", { name: "private-chat" });
    expect(route).toHaveAttribute("href", routeHref);
    expectResourceMarker(route, "route-1");
    const applicationSection = screen.getByRole("region", { name: "Application" });
    const catalogApplication = within(applicationSection).getByRole("link", { name: "OpenWebUI" });
    expect(catalogApplication).toHaveAttribute("href", applicationHref);
    expectResourceMarker(catalogApplication, "open-webui");
  });

  it("marks the application and placement mentions in their own sections", () => {
    render(
      <DeploymentApplicationInspector
        deployment={create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "deployment-1",
          applicationDeploymentName: "Team chat",
          placement: { siteNodeId: "node-51c47a4eca70f113", executionTargetId: "local-app" },
          snapshot: resolvedSnapshot(application),
        })}
        onRemove={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
        placementMachine={create(MachineSchema, {
          displayName: "gpu1",
          machineId: "node-51c47a4eca70f113",
        })}
        executionTargets={[
          create(ConfiguredExecutionTargetSchema, {
            displayName: "Docker applications",
            executionTargetId: "local-app",
            siteNodeDisplayName: "gpu1",
            siteNodeId: "node-51c47a4eca70f113",
          }),
        ]}
      />,
    );

    const applicationSection = screen.getByRole("region", { name: "Application" });
    expectResourceMarker(within(applicationSection).getByText("OpenWebUI"), "open-webui");
    const deploymentSection = screen.getByRole("region", { name: "Deployment" });
    expectResourceMarker(
      within(deploymentSection).getByText("gpu1 / Docker applications"),
      "node-51c47a4eca70f113",
    );
    expect(deploymentSection).not.toHaveTextContent("node-");
  });

  it("states an unassigned endpoint as an Access row value", () => {
    const deployment = create(ApplicationDeploymentSchema, {
      applicationDeploymentId: "deployment-1",
      applicationDeploymentName: "open-webui-1",
      snapshot: resolvedSnapshot(application),
      state: create(ApplicationDeploymentStateSchema, {
        lifecycleState: ApplicationLifecycleState.STARTING,
        readinessState: ApplicationReadinessState.NOT_READY,
      }),
    });

    render(
      <DeploymentApplicationInspector
        deployment={deployment}
        onRemove={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    expect(screen.queryByRole("link", { name: "Open" })).not.toBeInTheDocument();
    const access = screen.getByRole("region", { name: "Access" });
    expect(within(access).getByText("Not assigned")).toBeVisible();
  });

  describe("step bar", () => {
    const startSteps = [
      ApplicationDeploymentStep.PREPARING_IMAGE,
      ApplicationDeploymentStep.STARTING,
      ApplicationDeploymentStep.WAITING_FOR_ENDPOINT,
    ];

    function renderWithState(
      state: Parameters<typeof create<typeof ApplicationDeploymentStateSchema>>[1],
    ) {
      return render(
        <DeploymentApplicationInspector
          deployment={create(ApplicationDeploymentSchema, {
            applicationDeploymentId: "deployment-1",
            applicationDeploymentName: "open-webui-1",
            snapshot: resolvedSnapshot(application),
            state: create(ApplicationDeploymentStateSchema, state),
          })}
          onOpenChange={vi.fn()}
          onRemove={vi.fn()}
          open
          openerRef={createRef<HTMLElement>()}
          pending={false}
        />,
      );
    }

    it("shows the current step of a starting application", () => {
      renderWithState({
        lifecycleState: ApplicationLifecycleState.STARTING,
        readinessState: ApplicationReadinessState.NOT_READY,
        steps: { current: 1, steps: startSteps },
      });

      const bar = within(screen.getByRole("region", { name: "Status" })).getByRole("progressbar", {
        name: "Starting",
      });
      expect(bar).toHaveTextContent("Step 2 of 3");
      expect(
        [...bar.querySelectorAll('[data-slot="step-progress-segment"]')].map((segment) =>
          segment.getAttribute("data-state"),
        ),
      ).toEqual(["done", "current", "pending"]);
    });

    it("names the endpoint wait on the last step", () => {
      renderWithState({
        lifecycleState: ApplicationLifecycleState.STARTING,
        steps: { current: 2, steps: startSteps },
      });

      expect(screen.getByRole("progressbar", { name: "Waiting for endpoint" })).toHaveTextContent(
        "Step 3 of 3",
      );
    });

    it.each([
      ["ready", { lifecycleState: ApplicationLifecycleState.READY }],
      ["paused", { lifecycleState: ApplicationLifecycleState.PAUSED }],
      ["failed", { lifecycleState: ApplicationLifecycleState.FAILED }],
      [
        "stale",
        {
          lifecycleState: ApplicationLifecycleState.STARTING,
          observationStale: true,
          steps: { current: 1, steps: startSteps },
        },
      ],
    ])("hides the bar when the application is %s", (_name, state) => {
      renderWithState(state);

      expect(screen.queryByRole("progressbar")).not.toBeInTheDocument();
    });
  });

  it("renders four ordered sections and nothing outside a section", () => {
    render(
      <DeploymentApplicationInspector
        deployment={create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "deployment-1",
          applicationDeploymentName: "Team chat",
          placement: { siteNodeId: "local", executionTargetId: "local" },
          snapshot: resolvedSnapshot(application),
          state: create(ApplicationDeploymentStateSchema, {
            endpoint: { url: "http://100.64.0.7:49152" },
            lifecycleState: ApplicationLifecycleState.READY,
            readinessState: ApplicationReadinessState.READY,
          }),
        })}
        onOpenChange={vi.fn()}
        onRemove={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    expect(sectionNames()).toEqual(["Status", "Deployment", "Application", "Access"]);
    expect(nonSectionChildren()).toEqual([]);
  });

  it("shows the retained application notes after Access with line breaks kept", () => {
    const withNotes = create(ApplicationSchema, {
      ...application,
      notes: "Sign in with admin@dotcore.local / admin.\nPick the Dotcore Inference credential.",
    });
    render(
      <DeploymentApplicationInspector
        deployment={create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "deployment-1",
          applicationDeploymentName: "Workflows",
          placement: { siteNodeId: "local", executionTargetId: "local" },
          snapshot: resolvedSnapshot(withNotes),
          state: create(ApplicationDeploymentStateSchema, {
            endpoint: { url: "http://100.64.0.7:49152" },
            lifecycleState: ApplicationLifecycleState.READY,
            readinessState: ApplicationReadinessState.READY,
          }),
        })}
        onOpenChange={vi.fn()}
        onRemove={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    expect(sectionNames()).toEqual(["Status", "Deployment", "Application", "Access", "Notes"]);
    const notes = screen.getByRole("region", { name: "Notes" });
    expect(notes.querySelector("p")?.textContent).toBe(
      "Sign in with admin@dotcore.local / admin.\nPick the Dotcore Inference credential.",
    );
    expect(notes.querySelector("p")).toHaveClass("whitespace-pre-wrap");
  });

  it("shows the retained parameter values after Deployment, including advanced ones", () => {
    const withParameters = create(ApplicationSchema, {
      ...application,
      graph: create(ApplicationGraphSchema, {
        parameters: [
          {
            parameterId: "vram-mode",
            displayName: "VRAM mode",
            visibility: ParameterVisibility.ADVANCED,
            parameterType: {
              case: "stringEnum",
              value: { values: ["auto", "highvram"], defaultValue: "auto" },
            },
          },
          {
            parameterId: "preview",
            displayName: "Previews",
            visibility: ParameterVisibility.BASIC,
            parameterType: { case: "boolean", value: { defaultValue: false } },
          },
        ],
      }),
    });
    render(
      <DeploymentApplicationInspector
        deployment={create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "deployment-1",
          applicationDeploymentName: "Images",
          placement: { siteNodeId: "local", executionTargetId: "local" },
          snapshot: {
            ...resolvedSnapshot(withParameters),
            parameterValues: [
              { parameterId: "vram-mode", value: { case: "string", value: "highvram" } },
              { parameterId: "preview", value: { case: "boolean", value: false } },
            ],
          },
          state: create(ApplicationDeploymentStateSchema, {
            lifecycleState: ApplicationLifecycleState.READY,
            readinessState: ApplicationReadinessState.READY,
          }),
        })}
        onOpenChange={vi.fn()}
        onRemove={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    expect(sectionNames().slice(0, 3)).toEqual(["Status", "Deployment", "Parameters"]);
    expect(detailTexts(screen.getByRole("region", { name: "Parameters" }))).toEqual([
      ["VRAM mode", "highvram"],
      ["Previews", "defaultDisabled"],
    ]);
  });

  it("presents a remote deployment, its pinned image, and its direct endpoint", () => {
    const endpointUrl = "http://127.0.0.2:49152";
    render(
      <DeploymentApplicationInspector
        deployment={create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "deployment-1",
          applicationDeploymentName: "Team chat",
          placement: { siteNodeId: "worker", executionTargetId: "remote-app" },
          snapshot: resolvedSnapshot(application),
          nodeResolvedLaunchIdentities: [
            { selectionKey: "web", launchIdentity: `sha256:${"a".repeat(64)}` },
          ],
          state: create(ApplicationDeploymentStateSchema, {
            endpoint: { url: endpointUrl },
            lifecycleState: ApplicationLifecycleState.READY,
            readinessState: ApplicationReadinessState.READY,
          }),
        })}
        onOpenChange={vi.fn()}
        onRemove={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
        executionTargets={[remoteApplicationTarget("remote-app")]}
      />,
    );

    expect(sectionNames()).toEqual([
      "Status",
      "Deployment",
      "Application",
      "Pinned images",
      "Access",
    ]);
    const status = screen.getByRole("region", { name: "Status" });
    expect(within(status).getByText("Machine connection")).toBeVisible();
    expect(within(status).getByText("Connected")).toBeVisible();
    expect(within(status).getAllByText("Ready")[0]).toHaveAttribute("data-variant", "success");
    expect(within(status).queryByRole("status")).not.toBeInTheDocument();
    const pins = screen.getByRole("region", { name: "Pinned images" });
    expect(pins).toHaveTextContent("worker");
    expect(pins).not.toHaveTextContent("Local-development images");
    expect(within(pins).getByText("web")).toBeVisible();
    expect(within(pins).getByTitle(`sha256:${"a".repeat(64)}`)).toHaveTextContent(
      `sha256:${"a".repeat(12)}…aaaa`,
    );
    expect(
      within(screen.getByTestId("inspector-footer")).getByRole("link", { name: "Open" }),
    ).toHaveAttribute("href", endpointUrl);
  });

  it("marks a stale machine observation with the machine's last contact", () => {
    vi.useFakeTimers({ now: new Date("2026-10-01T12:00:00Z"), toFake: ["Date"] });
    render(
      <DeploymentApplicationInspector
        deployment={create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "deployment-1",
          applicationDeploymentName: "Team chat",
          placement: { siteNodeId: "worker", executionTargetId: "remote-app" },
          snapshot: resolvedSnapshot(application),
          state: create(ApplicationDeploymentStateSchema, {
            endpoint: { url: "http://127.0.0.2:49152" },
            lifecycleState: ApplicationLifecycleState.READY,
            observationStale: true,
            readinessState: ApplicationReadinessState.READY,
          }),
        })}
        onOpenChange={vi.fn()}
        onRemove={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
        executionTargets={[
          remoteApplicationTarget("remote-app", "worker", NodeObservationState.DISCONNECTED),
        ]}
        placementMachine={create(MachineSchema, {
          displayName: "gb10-host-2",
          machineId: "worker",
          observationFreshness: {
            receivedAt: timestampFromDate(new Date("2026-10-01T11:57:00Z")),
            state: NodeObservationState.DISCONNECTED,
          },
        })}
      />,
    );

    const status = screen.getByRole("region", { name: "Status" });
    expect(within(status).getByText("Disconnected")).toBeVisible();
    // The State row keeps the last reported word but never presents it as current.
    expect(within(status).getAllByText("Ready")[0]).toHaveAttribute("data-variant", "warning");
    expect(within(status).getByRole("status")).toHaveTextContent(
      "Machine stale. Showing the last state gb10-host-2 reported; last report 3 minutes ago. The state updates when the machine reports again.",
    );
    expect(within(status).getByText("3 minutes ago")).toHaveAttribute(
      "title",
      "2026-10-01 11:57:00 UTC",
    );
    // The last reported endpoint stays reachable on the machine.
    expect(screen.getByRole("link", { name: "Open" })).toBeVisible();
    vi.useRealTimers();
  });

  it("keeps remote presentation off a deployment on the site manager's machine", () => {
    render(
      <DeploymentApplicationInspector
        deployment={create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "deployment-1",
          applicationDeploymentName: "Team chat",
          placement: { siteNodeId: "local", executionTargetId: "local" },
          snapshot: resolvedSnapshot(application),
          state: create(ApplicationDeploymentStateSchema, {
            endpoint: { url: "http://100.64.0.7:49152" },
            lifecycleState: ApplicationLifecycleState.READY,
            readinessState: ApplicationReadinessState.READY,
          }),
        })}
        onOpenChange={vi.fn()}
        onRemove={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
        executionTargets={[applicationTarget("local")]}
      />,
    );

    expect(screen.queryByText("Machine connection")).not.toBeInTheDocument();
    expect(screen.queryByRole("status")).not.toBeInTheDocument();
  });

  it("drops the Application section when the deployment carries no snapshot", () => {
    render(
      <DeploymentApplicationInspector
        deployment={create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "deployment-1",
          applicationDeploymentName: "Team chat",
          placement: { siteNodeId: "local", executionTargetId: "local" },
          state: create(ApplicationDeploymentStateSchema, {
            endpoint: { url: "http://100.64.0.7:49152" },
            lifecycleState: ApplicationLifecycleState.READY,
            readinessState: ApplicationReadinessState.READY,
          }),
        })}
        onOpenChange={vi.fn()}
        onRemove={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    expect(sectionNames()).toEqual(["Status", "Deployment", "Access"]);
    expect(nonSectionChildren()).toEqual([]);
  });

  it("states the status once and shows the target only when the state does not imply it", () => {
    render(
      <DeploymentApplicationInspector
        deployment={create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "deployment-1",
          applicationDeploymentName: "Team chat",
          desiredState: ApplicationDeploymentDesiredState.PRESENT,
          placement: { siteNodeId: "local", executionTargetId: "local" },
          snapshot: resolvedSnapshot(application),
          state: create(ApplicationDeploymentStateSchema, {
            lifecycleState: ApplicationLifecycleState.READY,
            readinessState: ApplicationReadinessState.READY,
          }),
        })}
        onOpenChange={vi.fn()}
        onRemove={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    const status = screen.getByRole("region", { name: "Status" });
    expect([...status.querySelectorAll("dt")].map((label) => label.textContent)).toEqual(["State"]);
    expect(screen.queryByText("Desired state")).not.toBeInTheDocument();
    expect(screen.queryByText("Lifecycle")).not.toBeInTheDocument();
    expect(screen.queryByText("Readiness")).not.toBeInTheDocument();
    expect(screen.getAllByText("Ready")).toHaveLength(1);
  });

  it("shows the target when the deployment is meant to be present but is paused", () => {
    render(
      <DeploymentApplicationInspector
        deployment={create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "deployment-1",
          applicationDeploymentName: "Team chat",
          desiredState: ApplicationDeploymentDesiredState.PRESENT,
          placement: { siteNodeId: "local", executionTargetId: "local" },
          snapshot: resolvedSnapshot(application),
          state: create(ApplicationDeploymentStateSchema, {
            lifecycleState: ApplicationLifecycleState.PAUSED,
          }),
        })}
        onOpenChange={vi.fn()}
        onRemove={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    const status = screen.getByRole("region", { name: "Status" });
    expect([...status.querySelectorAll("dt")].map((label) => label.textContent)).toEqual([
      "State",
      "Desired",
    ]);
    expect(within(status).getByText("Paused")).toBeVisible();
    expect(within(status).getByText("Present")).toBeVisible();
  });

  it("renders no Status action when no log operations are supplied", () => {
    render(
      <DeploymentApplicationInspector
        deployment={create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "deployment-1",
          applicationDeploymentName: "Team chat",
          placement: { siteNodeId: "local", executionTargetId: "local" },
          snapshot: resolvedSnapshot(application),
        })}
        onOpenChange={vi.fn()}
        onRemove={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    const status = screen.getByRole("region", { name: "Status" });
    expect(within(status).queryByRole("button")).not.toBeInTheDocument();
  });

  it("states Not linked when a route-capable application has no route", () => {
    render(
      <DeploymentApplicationInspector
        deployment={create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "deployment-1",
          applicationDeploymentName: "Team chat",
          placement: { siteNodeId: "local", executionTargetId: "local" },
          snapshot: resolvedSnapshot(appWithOptionalRoute),
        })}
        onOpenChange={vi.fn()}
        onRemove={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    const section = screen.getByRole("region", { name: "Deployment" });
    const labels = [...section.querySelectorAll("dt")].map((label) => label.textContent);
    expect(labels.at(-1)).toBe("Inference route");
    expect([...section.querySelectorAll("dd")].at(-1)).toHaveTextContent(/^Not linked$/);
  });

  it("omits the Inference route row for an application without a route input", () => {
    render(
      <DeploymentApplicationInspector
        deployment={create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "deployment-1",
          applicationDeploymentName: "Team chat",
          placement: { siteNodeId: "local", executionTargetId: "local" },
          snapshot: resolvedSnapshot(application),
          state: create(ApplicationDeploymentStateSchema, {
            observedPlacement: { siteNodeId: "local", executionTargetId: "local" },
          }),
        })}
        executionTargets={[]}
        onOpenChange={vi.fn()}
        onRemove={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    const section = screen.getByRole("region", { name: "Deployment" });
    expect([...section.querySelectorAll("dt")].map((label) => label.textContent)).toEqual([
      "Deployment ID",
      "Name",
      "Placement",
      "GPU devices",
    ]);
    // Without a listed machine or target, the placement names the node and target IDs.
    expect(within(section).getByText("Placement").nextElementSibling).toHaveTextContent(
      /^local \/ local$/,
    );
  });

  it("folds a failure into a code row and one notice closing Status", () => {
    render(
      <DeploymentApplicationInspector
        deployment={create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "deployment-1",
          applicationDeploymentName: "Team chat",
          placement: { siteNodeId: "local", executionTargetId: "local" },
          snapshot: resolvedSnapshot(application),
          state: create(ApplicationDeploymentStateSchema, {
            failure: {
              code: "IMAGE_PULL",
              diagnostics: ["registry: manifest not found", "retry budget exhausted"],
              message: "Container image could not be pulled.",
            },
            lifecycleState: ApplicationLifecycleState.FAILED,
            readinessState: ApplicationReadinessState.NOT_READY,
          }),
        })}
        onOpenChange={vi.fn()}
        onRemove={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    const status = screen.getByRole("region", { name: "Status" });
    expect(within(status).getByText("Failure code")).toBeVisible();
    expect(within(status).getByText("IMAGE_PULL")).toBeVisible();
    const notices = within(status).getAllByRole("alert");
    expect(notices).toHaveLength(1);
    expect(notices[0]).toHaveTextContent("Container image could not be pulled.");
    expect(notices[0]).toHaveTextContent("registry: manifest not found");
    expect(notices[0]).toHaveTextContent("retry budget exhausted");
    expect(notices[0].querySelectorAll("div, p, ul, dl")).toHaveLength(0);
    expect(screen.queryByRole("region", { name: "Failure" })).not.toBeInTheDocument();
  });

  it("makes Open primary for an application with an endpoint and keeps Edit in the menu", async () => {
    const user = setupUser();
    render(
      <DeploymentApplicationInspector
        deployment={create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "deployment-1",
          desiredState: ApplicationDeploymentDesiredState.PRESENT,
          snapshot: resolvedSnapshot(application),
          state: create(ApplicationDeploymentStateSchema, {
            endpoint: { url: "http://100.64.0.7:49152" },
            lifecycleState: ApplicationLifecycleState.READY,
            readinessState: ApplicationReadinessState.READY,
          }),
        })}
        onOpenChange={vi.fn()}
        onPause={vi.fn()}
        onRemove={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    expect(await inspectorActionList(user)).toEqual({
      buttons: ["Open", "More actions"],
      menu: ["Edit", "Pause", "Teardown…"],
    });
    const open = screen.getByRole("link", { name: "Open" });
    expect(open).toHaveAttribute("href", "http://100.64.0.7:49152");
    expect(open).toHaveAttribute("target", "_blank");
  });

  it("uses a Cancel-first removal confirmation and suppresses it during teardown", async () => {
    const user = setupUser();
    const deployment = create(ApplicationDeploymentSchema, {
      applicationDeploymentId: "deployment-1",
      applicationDeploymentName: "open-webui-1",
      desiredState: ApplicationDeploymentDesiredState.PRESENT,
      snapshot: resolvedSnapshot(application),
      state: create(ApplicationDeploymentStateSchema, {
        lifecycleState: ApplicationLifecycleState.READY,
        readinessState: ApplicationReadinessState.READY,
      }),
    });
    const onRemove = vi.fn();
    render(
      <DeploymentApplicationInspector
        deployment={deployment}
        onRemove={onRemove}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    await chooseMoreAction(user, "Teardown…");
    const dialog = screen.getByRole("dialog");
    expect(within(dialog).getByRole("heading", { name: "Teardown deployment?" })).toBeVisible();
    expect(
      within(dialog).getByText(
        "Teardown deployment open-webui-1? The apps controller will stop and remove its recognized workload group. The deployment remains visible as Tearing down until cleanup completes. Its application data is deleted; the catalog application is kept.",
        { exact: true },
      ),
    ).toBeVisible();
    // Base UI's Dialog moves initial focus on a queued animation frame (floating-ui's
    // `enqueueFocus`), not synchronously with open, so wait for it to land on Cancel.
    await waitFor(() => {
      expect(screen.getByRole("button", { name: "Cancel" })).toHaveFocus();
    });
    await user.tab();
    expect(screen.getByRole("button", { name: "Teardown" })).toHaveFocus();
    await user.keyboard("{Escape}");
    expect(onRemove).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "More actions" })).toHaveFocus();

    await chooseMoreAction(user, "Teardown…");
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Teardown" }));
    expect(onRemove).toHaveBeenCalledTimes(1);
  });

  it("shows visible teardown and disables removal during teardown", async () => {
    const user = setupUser();
    const deployment = create(ApplicationDeploymentSchema, {
      applicationDeploymentId: "deployment-1",
      desiredState: ApplicationDeploymentDesiredState.TEARDOWN,
      snapshot: resolvedSnapshot(application),
    });
    render(
      <DeploymentApplicationInspector
        deployment={deployment}
        onRemove={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    expect(await inspectorActionList(user)).toEqual({ buttons: ["Edit (disabled)"], menu: [] });
    const status = screen.getByRole("region", { name: "Status" });
    expect(within(status).getByText("Tearing down")).toBeVisible();
    expect(within(status).queryByText("Teardown")).not.toBeInTheDocument();
  });

  it("closes confirmation and keeps removal suppressed when desired state changes", async () => {
    const user = setupUser();
    const present = create(ApplicationDeploymentSchema, {
      applicationDeploymentId: "deployment-1",
      applicationDeploymentName: "open-webui-1",
      desiredState: ApplicationDeploymentDesiredState.PRESENT,
      snapshot: resolvedSnapshot(application),
    });
    const tearingDown = create(ApplicationDeploymentSchema, {
      ...present,
      desiredState: ApplicationDeploymentDesiredState.TEARDOWN,
    });
    const view = render(
      <DeploymentApplicationInspector
        deployment={present}
        onRemove={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    await chooseMoreAction(user, "Teardown…");
    expect(screen.getByRole("dialog")).toBeVisible();
    view.rerender(
      <DeploymentApplicationInspector
        deployment={tearingDown}
        onRemove={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );
    await user.click(document.body);
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(await inspectorActionList(user)).toEqual({ buttons: ["Edit (disabled)"], menu: [] });
  });

  it("shows Pause for Present without confirmation and keeps Remove separate", async () => {
    const user = setupUser();
    const onPause = vi.fn();
    const onResume = vi.fn();
    const deployment = create(ApplicationDeploymentSchema, {
      applicationDeploymentId: "present-deployment",
      applicationDeploymentName: "open-webui-1",
      desiredState: ApplicationDeploymentDesiredState.PRESENT,
      snapshot: resolvedSnapshot(application),
      state: create(ApplicationDeploymentStateSchema, {
        lifecycleState: ApplicationLifecycleState.READY,
        readinessState: ApplicationReadinessState.READY,
      }),
    });

    render(
      <DeploymentApplicationInspector
        deployment={deployment}
        onPause={onPause}
        onRemove={vi.fn()}
        onResume={onResume}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    expect(await inspectorActionList(user)).toEqual({
      buttons: ["Edit", "More actions"],
      menu: ["Pause", "Teardown…"],
    });

    await chooseMoreAction(user, "Pause");

    expect(onPause).toHaveBeenCalledOnce();
    expect(onResume).not.toHaveBeenCalled();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it.each([
    ["queued", ApplicationLifecycleState.QUEUED],
    ["preparing", ApplicationLifecycleState.PREPARING],
    ["starting", ApplicationLifecycleState.STARTING],
    ["failed", ApplicationLifecycleState.FAILED],
    ["unavailable", ApplicationLifecycleState.UNAVAILABLE],
  ] as const)("keeps Pause enabled during %s startup state", async (_label, lifecycleState) => {
    const user = setupUser();
    const onPause = vi.fn();
    const deployment = create(ApplicationDeploymentSchema, {
      applicationDeploymentId: "present-deployment",
      applicationDeploymentName: "open-webui-1",
      desiredState: ApplicationDeploymentDesiredState.PRESENT,
      snapshot: resolvedSnapshot(application),
      state: create(ApplicationDeploymentStateSchema, {
        lifecycleState,
        readinessState: ApplicationReadinessState.NOT_READY,
      }),
    });

    render(
      <DeploymentApplicationInspector
        deployment={deployment}
        onPause={onPause}
        onRemove={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    expect((await inspectorActionList(user)).menu).toContain("Pause");
    await chooseMoreAction(user, "Pause");

    expect(onPause).toHaveBeenCalledOnce();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("shows Resume and Remove for Paused, preserves removal confirmation, suppresses stale endpoint access, and omits pause actions for Teardown", async () => {
    const user = setupUser();
    const paused = create(ApplicationDeploymentSchema, {
      applicationDeploymentId: "paused-deployment",
      applicationDeploymentName: "open-webui-1",
      desiredState: ApplicationDeploymentDesiredState.PAUSED,
      snapshot: resolvedSnapshot(application),
      state: create(ApplicationDeploymentStateSchema, {
        endpoint: { url: "http://100.64.0.7:49152" },
        lifecycleState: ApplicationLifecycleState.PAUSED,
        readinessState: ApplicationReadinessState.READY,
      }),
    });
    const teardown = create(ApplicationDeploymentSchema, {
      applicationDeploymentId: "teardown-deployment",
      applicationDeploymentName: "open-webui-1",
      desiredState: ApplicationDeploymentDesiredState.TEARDOWN,
      snapshot: resolvedSnapshot(application),
      state: create(ApplicationDeploymentStateSchema, {
        lifecycleState: ApplicationLifecycleState.STOPPING,
      }),
    });

    const { unmount } = render(
      <DeploymentApplicationInspector
        deployment={paused}
        onPause={vi.fn()}
        onRemove={vi.fn()}
        onResume={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    expect(await inspectorActionList(user)).toEqual({
      buttons: ["Resume", "More actions"],
      menu: ["Edit", "Teardown…"],
    });
    expect(screen.queryByRole("link", { name: "Open" })).not.toBeInTheDocument();
    expect(screen.queryByText("http://100.64.0.7:49152")).not.toBeInTheDocument();

    await chooseMoreAction(user, "Teardown…");
    expect(screen.getByRole("dialog")).toHaveTextContent("Teardown deployment?");
    await user.keyboard("{Escape}");

    unmount();
    render(
      <DeploymentApplicationInspector
        deployment={teardown}
        onPause={vi.fn()}
        onRemove={vi.fn()}
        onResume={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    expect(await inspectorActionList(user)).toEqual({ buttons: ["Edit (disabled)"], menu: [] });
  });

  it("disables conflicting actions and Open while an accepted pause is still pausing", async () => {
    const user = setupUser();
    const deployment = create(ApplicationDeploymentSchema, {
      applicationDeploymentId: "pausing-deployment",
      applicationDeploymentName: "open-webui-1",
      desiredState: ApplicationDeploymentDesiredState.PAUSED,
      snapshot: resolvedSnapshot(application),
      state: create(ApplicationDeploymentStateSchema, {
        endpoint: { url: "http://100.64.0.7:49152" },
        lifecycleState: ApplicationLifecycleState.PAUSING,
        readinessState: ApplicationReadinessState.READY,
      }),
    });

    render(
      <DeploymentApplicationInspector
        deployment={deployment}
        onPause={vi.fn()}
        onRemove={vi.fn()}
        onResume={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    expect(await inspectorActionList(user)).toEqual({
      buttons: ["Resume (disabled)", "More actions"],
      menu: ["Edit", "Teardown… (disabled)"],
    });
    expect(screen.getAllByText("Pausing")[0]).toBeVisible();
  });

  it("opens the log viewer from the Status action without changing lifecycle controls", async () => {
    const onOpenLogs = vi.fn();
    const logs: DeploymentLogOperations = {
      follow: vi.fn(async function* () {}),
      read: vi.fn().mockResolvedValue({
        entries: [],
        historyState: "no-older-output-observed",
        latestPositions: [],
        responseTruncated: false,
        sources: [],
      }),
    };
    const user = setupUser();
    const deployment = create(ApplicationDeploymentSchema, {
      applicationDeploymentId: "deployment-with-logs",
      desiredState: ApplicationDeploymentDesiredState.PRESENT,
      applicationDeploymentName: "open-webui-1",
      snapshot: resolvedSnapshot(application),
      state: create(ApplicationDeploymentStateSchema, {
        lifecycleState: ApplicationLifecycleState.READY,
      }),
    });

    render(
      <DeploymentApplicationInspector
        deployment={deployment}
        logs={logs}
        onOpenChange={vi.fn()}
        onOpenLogs={onOpenLogs}
        onRemove={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    const status = screen.getByRole("region", { name: "Status" });
    expect(status.parentElement).toHaveClass("inspector-sections");
    expect(screen.queryByRole("region", { name: "Logs" })).not.toBeInTheDocument();
    expect(
      screen.queryByText("No Docker-retained log output is available."),
    ).not.toBeInTheDocument();
    await user.click(within(status).getByRole("button", { name: "Logs" }));
    expect(onOpenLogs).toHaveBeenCalledTimes(1);
    expect(await inspectorActionList(user)).toEqual({
      buttons: ["Edit", "More actions"],
      menu: ["Pause", "Teardown…"],
    });
  });

  it("keeps deployment actions available in the narrow full-screen inspector", () => {
    setViewportWidth(500);
    const deployment = create(ApplicationDeploymentSchema, {
      applicationDeploymentId: "narrow-deployment",
      applicationDeploymentName: "open-webui-1",
      desiredState: ApplicationDeploymentDesiredState.PAUSED,
      snapshot: resolvedSnapshot(application),
      state: create(ApplicationDeploymentStateSchema, {
        lifecycleState: ApplicationLifecycleState.PAUSED,
      }),
    });

    render(
      <DeploymentApplicationInspector
        deployment={deployment}
        onPause={vi.fn()}
        onRemove={vi.fn()}
        onResume={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    expect(screen.getByRole("dialog", { name: "OpenWebUI" })).toBeVisible();
    expect(screen.getByTestId("inspector-content")).toHaveClass("inset-0");
    const footer = screen.getByTestId("inspector-footer");
    expect(within(footer).getByRole("button", { name: "Resume" })).toBeVisible();
    expect(within(footer).getByRole("button", { name: "More actions" })).toBeVisible();
  });
  it("lists the devices the deployment retained, next to the placement", () => {
    renderGpuInspector(["nvidia.com/gpu=0", "nvidia.com/gpu=1"]);

    const labels = detailTexts(screen.getByRole("complementary")).map(([label]) => label);
    const placement = labels.indexOf("Placement");
    expect(labels[placement + 1]).toBe("GPU devices");
    expect(detailTexts(screen.getByRole("complementary"))[placement + 1]).toEqual([
      "GPU devices",
      "nvidia.com/gpu=0, nvidia.com/gpu=1",
    ]);
  });

  it("says none when the deployment retained no devices", () => {
    renderGpuInspector([]);

    expect(detailTexts(screen.getByRole("complementary"))).toEqual(
      expect.arrayContaining([["GPU devices", "none"]]),
    );
    expect(screen.queryByText(/memory fit|estimat/i)).toBeNull();
  });

  it("omits the GPU devices row for a deployment without a snapshot", () => {
    render(
      <DeploymentApplicationInspector
        deployment={create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "deployment-1",
          applicationDeploymentName: "open-webui-1",
          placement: { siteNodeId: "local", executionTargetId: "local" },
        })}
        onRemove={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    expect(detailTexts(screen.getByRole("complementary")).map(([label]) => label)).not.toContain(
      "GPU devices",
    );
  });

  function renderGpuInspector(acceleratorDeviceIds: string[]) {
    render(
      <DeploymentApplicationInspector
        deployment={create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "deployment-1",
          applicationDeploymentName: "open-webui-1",
          snapshot: { ...resolvedSnapshot(application), acceleratorDeviceIds },
          placement: { siteNodeId: "local", executionTargetId: "local" },
        })}
        onRemove={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );
  }
});

describe("Accelerator memory section", () => {
  function service(
    name: string,
    acceleratorMemory?: AcceleratorMemoryObservation,
  ): ApplicationServiceAcceleratorMemory {
    return create(ApplicationServiceAcceleratorMemorySchema, {
      acceleratorMemory,
      serviceName: name,
    });
  }

  function deployment(
    units: ApplicationServiceAcceleratorMemory[],
    lifecycleState = ApplicationLifecycleState.RUNNING,
  ) {
    return create(ApplicationDeploymentSchema, {
      applicationDeploymentId: "deployment-1",
      applicationDeploymentName: "open-webui-1",
      serviceAcceleratorMemory: units,
      snapshot: resolvedSnapshot(application),
      state: create(ApplicationDeploymentStateSchema, { lifecycleState }),
      placement: { siteNodeId: "local", executionTargetId: "local" },
    });
  }

  it("reads each unit's own result distinctly and leaves uncovered units out", () => {
    const rendered = render(
      <DeploymentApplicationInspector
        deployment={deployment([
          service(
            "web",
            create(AcceleratorMemoryObservationSchema, {
              observedAt: { nanos: 0, seconds: BigInt(Math.floor(Date.now() / 1000)) },
              result: {
                case: "available",
                value: {
                  devices: [
                    { deviceId: "GPU-a", managedNodeId: "local", usedMemoryBytes: 2_684_354_560n },
                  ],
                },
              },
            }),
          ),
          service(
            "cache",
            create(AcceleratorMemoryObservationSchema, {
              result: {
                case: "unavailable",
                value: {
                  code: "PROCESS_NOT_OBSERVED",
                  message: "An authoritative workload process was not observed on an accelerator.",
                },
              },
            }),
          ),
          service(
            "api",
            create(AcceleratorMemoryObservationSchema, {
              result: { case: "notApplicable", value: {} },
            }),
          ),
          service("db"),
        ])}
        onRemove={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    // The section sits after the Deployment section and before the Application section.
    expect(sectionNames()).toEqual([
      "Status",
      "Deployment",
      "Accelerator memory",
      "Application",
      "Access",
    ]);

    const section = screen.getByRole("region", { name: "Accelerator memory" });
    expect(within(section).getByText("2.5 GiB")).toBeVisible();
    expect(within(section).getByText("process not seen using a GPU")).toBeVisible();
    expect(within(section).getByText("not applicable")).toBeVisible();
    // The unit the projection does not cover renders nothing, never a zero.
    expect(within(section).queryByText("db")).not.toBeInTheDocument();
    expect(within(section).queryByText(/0 bytes/)).not.toBeInTheDocument();
    rendered.unmount();
  });

  it("shows a stale reading dimmed with its observation time as text", () => {
    const observed = create(AcceleratorMemoryObservationSchema, {
      observedAt: { nanos: 0, seconds: BigInt(Math.floor(Date.now() / 1000) - 600) },
      refresh: { outcome: { case: "failed", value: { code: "COLLECTION_FAILED", message: "" } } },
      result: {
        case: "available",
        value: {
          devices: [{ deviceId: "GPU-a", managedNodeId: "local", usedMemoryBytes: 1_073_741_824n }],
        },
      },
    });
    render(
      <DeploymentApplicationInspector
        deployment={deployment([service("web", observed)])}
        onRemove={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    const section = screen.getByRole("region", { name: "Accelerator memory" });
    expect(within(section).getByText("1 GiB")).toBeVisible();
    expect(within(section).getByText("stale, observed 10 minutes ago")).toBeVisible();
    expect(within(section).getByText("1 GiB").parentElement).toHaveAttribute("data-stale", "true");
  });

  it("reads every unit of a paused deployment as not running", () => {
    render(
      <DeploymentApplicationInspector
        deployment={deployment(
          [
            service(
              "web",
              create(AcceleratorMemoryObservationSchema, {
                result: { case: "notApplicable", value: {} },
              }),
            ),
            service(
              "cache",
              create(AcceleratorMemoryObservationSchema, {
                result: { case: "notApplicable", value: {} },
              }),
            ),
          ],
          ApplicationLifecycleState.PAUSED,
        )}
        onRemove={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    const section = screen.getByRole("region", { name: "Accelerator memory" });
    expect(within(section).getAllByText("not running")).toHaveLength(2);
    expect(within(section).queryByText("not applicable")).not.toBeInTheDocument();
  });

  it("reads a unit spanning several devices as one line per device", () => {
    render(
      <DeploymentApplicationInspector
        deployment={deployment([
          service(
            "web",
            create(AcceleratorMemoryObservationSchema, {
              observedAt: { nanos: 0, seconds: BigInt(Math.floor(Date.now() / 1000)) },
              result: {
                case: "available",
                value: {
                  devices: [
                    { deviceId: "GPU-a", managedNodeId: "local", usedMemoryBytes: 1_073_741_824n },
                    { deviceId: "GPU-b", managedNodeId: "local", usedMemoryBytes: 2_147_483_648n },
                  ],
                },
              },
            }),
          ),
        ])}
        onRemove={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    const section = screen.getByRole("region", { name: "Accelerator memory" });
    expect(within(section).getByText("GPU-a: 1 GiB")).toBeVisible();
    expect(within(section).getByText("GPU-b: 2 GiB")).toBeVisible();
  });

  it("draws no section when the projection covers no unit", () => {
    render(
      <DeploymentApplicationInspector
        deployment={deployment([service("web")])}
        onRemove={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    expect(screen.queryByRole("region", { name: "Accelerator memory" })).not.toBeInTheDocument();
  });
});
