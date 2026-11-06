import { create } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";
import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { RouteClient } from "@/api/route-client";
import {
  ApplicationDeploymentDesiredState,
  ApplicationDeploymentSchema,
} from "@/gen/dotcore/application/v1/deployment_pb";
import {
  CreateApplicationDeploymentResponseSchema,
  type ListApplicationDeploymentsResponse,
  ListApplicationDeploymentsResponseSchema,
} from "@/gen/dotcore/application/v1/service_pb";
import {
  ApplicationLifecycleState,
  ApplicationReadinessState,
} from "@/gen/dotcore/application/v1/state_pb";
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
import { ListApplicationsResponseSchema } from "@/gen/dotcore/catalog/v1/service_pb";
import {
  InferenceRouteAvailabilitySchema,
  InferenceRouteSchema,
  InferenceRouteUnavailabilityReason,
  InferenceRouteViewSchema,
  ListRoutesResponseSchema,
} from "@/gen/dotcore/inference/v1/routes_pb";
import { ListConfiguredExecutionTargetsResponseSchema } from "@/gen/dotcore/site/v1/service_pb";
import { expectResourceMarker } from "@/test/resource-marker";

import { applicationInspectorPanelId } from "./application-inspector-panel";
import {
  app,
  applicationTarget,
  clientFor,
  currentLocation,
  deployment,
  renderAppsSection,
  routeClientFor,
  routeInputApp,
} from "./applications-section-test-support";

afterEach(() => {
  vi.useRealTimers();
});

// Turns Advanced on and moves from the first step to Target.
async function openAdvanced(
  user: ReturnType<typeof userEvent.setup>,
  scope: Pick<typeof screen, "getByRole"> = screen,
) {
  await user.click(scope.getByRole("checkbox", { name: "Advanced" }));
  await user.click(scope.getByRole("button", { name: "Next" }));
}

function createCreateResponse(deploymentValue: ReturnType<typeof deployment>) {
  return create(CreateApplicationDeploymentResponseSchema, { deployment: deploymentValue });
}

describe("AppCatalogPage", () => {
  it("renders a provisioned Hermes catalog application through the generic catalog path", async () => {
    const client = clientFor([app("open-webui", "OpenWebUI"), app("hermes-app", "Hermes")], []);

    renderAppsSection({ client, initialEntry: "/apps/catalog" });

    expect(await screen.findByRole("row", { name: "Hermes" })).toBeVisible();
    expect(client.catalog.listApplications).toHaveBeenCalledWith({}, expect.anything());
  });

  it("describes a catalog application by its version, with no status badge", async () => {
    const client = clientFor([routeInputApp()], []);

    renderAppsSection({ client, initialEntry: "/apps/catalog" });

    const row = await screen.findByRole("row", { name: "OpenWebUI" });
    const cells = within(row).getAllByRole("cell");
    expect(cells.map((cell) => cell.textContent)).toEqual(["OpenWebUI", "1.0"]);
    expect(cells[0].querySelectorAll('[data-slot="badge"]')).toHaveLength(0);
  });

  it("opens deployment controls without changing the selected inspector and supports repeat opens", async () => {
    const user = userEvent.setup();
    const client = clientFor([app("open-webui", "OpenWebUI")], []);

    renderAppsSection({ client, initialEntry: "/apps/catalog" });
    const opener = await screen.findByRole("row", { name: "OpenWebUI" });
    await user.click(opener);
    expect(screen.getByRole("complementary", { name: "OpenWebUI" })).toBeVisible();

    await user.click(
      within(screen.getByRole("complementary", { name: "OpenWebUI" })).getByRole("button", {
        name: "Deploy",
      }),
    );
    expect(screen.getByRole("dialog", { name: "OpenWebUI — Deploy application" })).toBeVisible();
    expect(screen.getByTestId("inspector-content")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    expect(
      screen.queryByRole("dialog", { name: "OpenWebUI — Deploy application" }),
    ).not.toBeInTheDocument();
    await user.click(
      within(screen.getByRole("complementary", { name: "OpenWebUI" })).getByRole("button", {
        name: "Deploy",
      }),
    );
    expect(screen.getByRole("dialog", { name: "OpenWebUI — Deploy application" })).toBeVisible();
  });

  it("resets the deployment draft after an idle dialog is dismissed and reopened", async () => {
    const user = userEvent.setup();
    const catalogApplication = create(ApplicationSchema, {
      ...routeInputApp(),
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
    const route = create(InferenceRouteViewSchema, {
      requestKinds: [InferenceRequestKind.CHAT],
      route: create(InferenceRouteSchema, {
        inferenceRouteId: "route-id",
        name: "chat",
        enabled: true,
        generation: 4n,
      }),
      availability: create(InferenceRouteAvailabilitySchema, {
        available: true,
        availableInputModalities: [ModelInputModality.TEXT],
      }),
    });
    const client = clientFor([catalogApplication], []);
    renderAppsSection({
      client,
      routeClient: routeClientFor([route]),
      initialEntry: "/apps/catalog",
    });

    await user.click(await screen.findByRole("row", { name: "OpenWebUI" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    const dialog = screen.getByRole("dialog", { name: "OpenWebUI — Deploy application" });
    await user.selectOptions(
      within(dialog).getByRole("combobox", { name: "Inference route" }),
      "route-id",
    );
    await openAdvanced(user, within(dialog));
    expect(within(dialog).getByLabelText("Execution target")).toHaveValue("local");
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    await user.clear(within(dialog).getByLabelText("Deployment name"));
    await user.type(within(dialog).getByLabelText("Deployment name"), "Temporary name");
    await user.click(screen.getByRole("button", { name: "Cancel" }));

    await user.click(screen.getByRole("button", { name: "Deploy" }));
    const reopenedDialog = screen.getByRole("dialog", { name: "OpenWebUI — Deploy application" });
    expect(within(reopenedDialog).getByRole("combobox", { name: "Inference route" })).toHaveValue(
      "",
    );
    expect(within(reopenedDialog).queryByLabelText("Deployment name")).not.toBeInTheDocument();
    await openAdvanced(user, within(reopenedDialog));
    expect(within(reopenedDialog).getByLabelText("Execution target")).toHaveValue("local");
    await user.click(within(reopenedDialog).getByRole("button", { name: "Next" }));
    expect(within(reopenedDialog).getByLabelText("Deployment name")).toHaveValue("OpenWebUI");
    await user.click(within(reopenedDialog).getByRole("checkbox", { name: "Advanced" }));
    await user.click(within(reopenedDialog).getByRole("button", { name: "Deploy" }));

    expect(client.application.createApplicationDeployment).toHaveBeenCalledWith({
      applicationId: "open-webui",
      applicationDeploymentName: "OpenWebUI",
      placement: { siteNodeId: "local", executionTargetId: "local" },
      parameterValues: [],
    });
  });

  it("sends the selected machine and lands on the accepted deployment on /apps, where Back returns to the Catalog", async () => {
    const user = userEvent.setup();
    const catalogApplication = app("open-webui", "OpenWebUI");
    const accepted = deployment(
      catalogApplication,
      ApplicationLifecycleState.QUEUED,
      ApplicationReadinessState.NOT_READY,
      "accepted-deployment",
    );
    const client = clientFor([catalogApplication], []);
    client.application.createApplicationDeployment = vi.fn(() =>
      Promise.resolve(create(CreateApplicationDeploymentResponseSchema, { deployment: accepted })),
    );

    const { router } = renderAppsSection({ client, initialEntry: "/apps/catalog" });
    await user.click(await screen.findByRole("row", { name: "OpenWebUI" }));
    await user.click(
      within(screen.getByRole("complementary", { name: "OpenWebUI" })).getByRole("button", {
        name: "Deploy",
      }),
    );
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Next" }));
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Deploy" }));

    expect(client.application.createApplicationDeployment).toHaveBeenCalledWith({
      applicationId: "open-webui",
      applicationDeploymentName: "OpenWebUI",
      placement: { siteNodeId: "local", executionTargetId: "local" },
      parameterValues: [],
    });
    await waitFor(() =>
      expect(currentLocation(router)).toBe("/apps?panel=apps%2Fdeployment%3Aaccepted-deployment"),
    );
    expect(await screen.findByText("accepted-deployment")).toBeVisible();
    expect(
      within(screen.getByRole("list", { name: "Installed applications" })).getByRole("article", {
        name: "OpenWebUI deployment",
      }),
    ).toBeVisible();

    await act(() => router.navigate(-1));
    expect(currentLocation(router)).toBe("/apps/catalog?panel=app-catalog%2Fcatalog%3Aopen-webui");
    expect(await screen.findByRole("region", { name: "Catalog application" })).toBeVisible();
  });

  it("closes the accepted deployment panel onto /apps", async () => {
    const user = userEvent.setup();
    const catalogApplication = app("open-webui", "OpenWebUI");
    const accepted = deployment(
      catalogApplication,
      ApplicationLifecycleState.QUEUED,
      ApplicationReadinessState.NOT_READY,
      "accepted-deployment",
    );
    const client = clientFor([catalogApplication], []);
    client.application.createApplicationDeployment = vi.fn(() =>
      Promise.resolve(createCreateResponse(accepted)),
    );

    const { router } = renderAppsSection({ client, initialEntry: "/apps/catalog" });
    await user.click(await screen.findByRole("row", { name: "OpenWebUI" }));
    await user.click(
      within(screen.getByRole("complementary", { name: "OpenWebUI" })).getByRole("button", {
        name: "Deploy",
      }),
    );
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Next" }));
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Deploy" }));
    await waitFor(() =>
      expect(currentLocation(router)).toBe("/apps?panel=apps%2Fdeployment%3Aaccepted-deployment"),
    );

    // The location moves before the panel re-renders; closing the stale catalog content is a no-op.
    const panel = screen.getByRole("complementary", { name: "OpenWebUI" });
    await within(panel).findByRole("heading", { name: "Status" });
    await user.click(within(panel).getByRole("button", { name: "Close inspector" }));

    await waitFor(() => expect(currentLocation(router)).toBe("/apps"));
    expect(screen.queryByRole("complementary")).not.toBeInTheDocument();
    expect(
      await screen.findByRole("button", { name: "View details for OpenWebUI deployment" }),
    ).toBeVisible();
  });

  it("stays where the operator went when a deploy is accepted after leaving the Catalog", async () => {
    const user = userEvent.setup();
    const catalogApplication = app("open-webui", "OpenWebUI");
    const client = clientFor([catalogApplication], []);
    const accepted = deferred<ReturnType<typeof createCreateResponse>>();
    client.application.createApplicationDeployment = vi.fn(() => accepted.promise);

    const { router } = renderAppsSection({ client, initialEntry: "/apps/catalog" });
    await user.click(await screen.findByRole("row", { name: "OpenWebUI" }));
    await user.click(
      within(screen.getByRole("complementary", { name: "OpenWebUI" })).getByRole("button", {
        name: "Deploy",
      }),
    );
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Next" }));
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Deploy" }));
    await act(() => router.navigate("/apps"));
    // Later deployment reads stay pending, so the installed list shows the accepted overlay.
    client.application.listApplicationDeployments = vi.fn(
      () => new Promise<ListApplicationDeploymentsResponse>(() => undefined),
    );
    await act(async () => {
      accepted.resolve(
        createCreateResponse(
          deployment(
            catalogApplication,
            ApplicationLifecycleState.QUEUED,
            ApplicationReadinessState.NOT_READY,
            "accepted-deployment",
          ),
        ),
      );
      await accepted.promise;
    });

    expect(await screen.findByRole("article", { name: "OpenWebUI deployment" })).toBeVisible();
    expect(currentLocation(router)).toBe("/apps");
  });

  it("keeps the accepted deployment on /apps through a stale deployment read", async () => {
    const user = userEvent.setup();
    const catalogApplication = app("open-webui", "OpenWebUI");
    const accepted = deployment(
      catalogApplication,
      ApplicationLifecycleState.STARTING,
      ApplicationReadinessState.NOT_READY,
      "accepted-deployment",
    );
    let resolveRefresh!: (response: ListApplicationDeploymentsResponse) => void;
    const refresh = new Promise<ListApplicationDeploymentsResponse>((resolve) => {
      resolveRefresh = resolve;
    });
    const client = clientFor([catalogApplication], []);
    let reads = 0;
    client.application.listApplicationDeployments = vi.fn(() => {
      reads += 1;
      return reads === 1
        ? Promise.resolve(create(ListApplicationDeploymentsResponseSchema))
        : refresh;
    });
    client.application.createApplicationDeployment = vi.fn(() =>
      Promise.resolve(createCreateResponse(accepted)),
    );

    renderAppsSection({ client, initialEntry: "/apps/catalog" });
    await user.click(await screen.findByRole("row", { name: "OpenWebUI" }));
    await user.click(
      within(screen.getByRole("complementary", { name: "OpenWebUI" })).getByRole("button", {
        name: "Deploy",
      }),
    );
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Next" }));
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Deploy" }));
    expect(await screen.findByText("accepted-deployment")).toBeVisible();

    resolveRefresh(create(ListApplicationDeploymentsResponseSchema));
    await act(async () => {});
    expect(screen.getByText("accepted-deployment")).toBeVisible();
  });

  it("supports keyboard row selection and accessible inspector close", async () => {
    const user = userEvent.setup();
    const catalogApplication = app("open-webui", "OpenWebUI");
    renderAppsSection({
      client: clientFor([catalogApplication], []),
      initialEntry: "/apps/catalog",
    });

    const row = await screen.findByRole("row", { name: "OpenWebUI" });
    row.focus();
    fireEvent.keyDown(row, { key: "Enter" });
    expect(screen.getByRole("complementary", { name: "OpenWebUI" })).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Close inspector" }));
    expect(row).toHaveFocus();
  });

  it("clears only the catalog alert after a successful retry", async () => {
    vi.useFakeTimers();
    const application = app("open-webui", "OpenWebUI");
    const client = clientFor([], []);
    let catalogReads = 0;
    client.catalog.listApplications = vi.fn(() => {
      catalogReads += 1;
      return catalogReads === 1
        ? Promise.reject(new Error("catalog offline"))
        : Promise.resolve(create(ListApplicationsResponseSchema, { applications: [application] }));
    });

    renderAppsSection({ client, initialEntry: "/apps/catalog" });
    await act(async () => {});
    expect(screen.getByRole("alert", { name: "Catalog status" })).toBeVisible();
    expect(screen.queryByRole("row", { name: "OpenWebUI" })).not.toBeInTheDocument();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
      await Promise.resolve();
    });
    expect(screen.queryByRole("alert", { name: "Catalog status" })).not.toBeInTheDocument();
    expect(screen.getByRole("row", { name: "OpenWebUI" })).toBeVisible();
  });

  it("keeps the catalog visible but disables create when deployments fail", async () => {
    const user = userEvent.setup();
    const catalogApplication = app("open-webui", "OpenWebUI");
    const client = clientFor([catalogApplication], []);
    client.application.listApplicationDeployments = vi.fn(() =>
      Promise.reject(new Error("deployments offline")),
    );

    renderAppsSection({ client, initialEntry: "/apps/catalog" });

    await user.click(await screen.findByRole("row", { name: "OpenWebUI" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Next" }));
    expect(
      within(screen.getByRole("dialog")).getByRole("button", { name: "Deploy" }),
    ).toBeDisabled();
  });

  it("restores exact target selection after deployments recover", async () => {
    vi.useFakeTimers();
    const application = app("open-webui", "OpenWebUI");
    const client = clientFor([application], []);
    let deploymentReads = 0;
    client.application.listApplicationDeployments = vi.fn(() => {
      deploymentReads += 1;
      return deploymentReads === 1
        ? Promise.reject(new Error("deployments offline"))
        : Promise.resolve(create(ListApplicationDeploymentsResponseSchema));
    });

    renderAppsSection({ client, initialEntry: "/apps/catalog" });
    await act(async () => {});
    fireEvent.click(screen.getByRole("row", { name: "OpenWebUI" }));
    fireEvent.click(screen.getByRole("button", { name: "Deploy" }));
    const dialog = screen.getByRole("dialog");
    fireEvent.click(within(dialog).getByRole("button", { name: "Next" }));
    expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeDisabled();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
      await Promise.resolve();
    });
    expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeEnabled();
    expect(within(dialog).queryByLabelText("Execution target")).not.toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeEnabled();
  });

  it("retains stale execution targets for display without authorizing create, then recovers", async () => {
    vi.useFakeTimers();
    const catalogApplication = app("open-webui", "OpenWebUI");
    const queued = deployment(
      catalogApplication,
      ApplicationLifecycleState.QUEUED,
      ApplicationReadinessState.NOT_READY,
    );
    const client = clientFor([catalogApplication], [queued]);
    let machineReads = 0;
    client.site.listConfiguredExecutionTargets = vi.fn(() => {
      machineReads += 1;
      return machineReads === 2
        ? Promise.reject(new Error("machines offline"))
        : Promise.resolve(
            create(ListConfiguredExecutionTargetsResponseSchema, {
              executionTargets: [applicationTarget("local", "local")],
            }),
          );
    });

    renderAppsSection({ client, initialEntry: "/apps/catalog" });
    await act(async () => {});
    fireEvent.click(screen.getByRole("row", { name: "OpenWebUI" }));
    fireEvent.click(screen.getByRole("button", { name: "Deploy" }));
    const dialog = screen.getByRole("dialog");
    await act(async () => {});
    fireEvent.click(within(dialog).getByRole("button", { name: "Next" }));
    expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeEnabled();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
    });
    expect(screen.getByRole("alert", { name: "Execution target status" })).toHaveTextContent(
      "Application execution targets are stale",
    );
    expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeDisabled();
    fireEvent.click(within(dialog).getByRole("checkbox", { name: "Advanced" }));
    fireEvent.click(within(dialog).getByRole("button", { name: "Back" }));
    expect(
      within(dialog).getByRole("option", {
        name: "local / Docker applications — Unknown platform",
      }),
    ).toBeVisible();
    expect(within(dialog).getByRole("button", { name: "Next" })).toBeDisabled();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
    });
    expect(
      screen.queryByRole("alert", { name: "Execution target status" }),
    ).not.toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Next" })).toBeEnabled();
  });

  it("controls the application inspector panel from the selected row's aria-controls", async () => {
    const catalogApplication = app("open-webui", "OpenWebUI");
    renderAppsSection({
      client: clientFor([catalogApplication], []),
      initialEntry: "/apps/catalog",
    });

    const row = await screen.findByRole("row", { name: "OpenWebUI" });
    fireEvent.click(row);

    const panel = await screen.findByRole("complementary", { name: "OpenWebUI" });
    expect(panel.id).toBe(applicationInspectorPanelId);
    expect(row).toHaveAttribute("aria-controls", applicationInspectorPanelId);
  });

  it("clears the selected row when its inspector closes", async () => {
    const catalogApplication = app("open-webui", "OpenWebUI");
    renderAppsSection({
      client: clientFor([catalogApplication], []),
      initialEntry: "/apps/catalog",
    });

    const row = await screen.findByRole("row", { name: "OpenWebUI" });
    fireEvent.click(row);
    expect(row).toHaveAttribute("data-state", "selected");

    fireEvent.click(screen.getByRole("button", { name: "Close inspector" }));

    await waitFor(() => expect(row).not.toHaveAttribute("data-state"));
  });

  it("offers every chat route as a choice labelled with its health", async () => {
    const user = userEvent.setup();
    const catalogApplication = routeInputApp();
    const routes = [
      create(InferenceRouteViewSchema, {
        requestKinds: [InferenceRequestKind.CHAT],
        route: create(InferenceRouteSchema, {
          inferenceRouteId: "chat",
          name: "chat",
          enabled: true,
          generation: 7n,
          fallbackGroups: [{ modelDeploymentIds: ["chat-model"] }],
        }),
        availability: create(InferenceRouteAvailabilitySchema, {
          available: true,
          availableInputModalities: [ModelInputModality.TEXT],
          fallbackGroups: [
            {
              fallbackGroupIndex: 0,
              members: [
                {
                  modelDeploymentId: "chat-model",
                  eligible: true,
                  availableInputModalities: [ModelInputModality.TEXT],
                },
              ],
            },
          ],
        }),
      }),
      create(InferenceRouteViewSchema, {
        requestKinds: [InferenceRequestKind.CHAT],
        route: create(InferenceRouteSchema, {
          inferenceRouteId: "disabled",
          name: "disabled",
          enabled: false,
          generation: 2n,
        }),
        availability: create(InferenceRouteAvailabilitySchema, {
          available: false,
          exclusionReasons: [InferenceRouteUnavailabilityReason.DISABLED],
        }),
      }),
      create(InferenceRouteViewSchema, {
        requestKinds: [InferenceRequestKind.CHAT],
        route: create(InferenceRouteSchema, {
          inferenceRouteId: "vision",
          name: "vision",
          enabled: true,
          generation: 3n,
        }),
        availability: create(InferenceRouteAvailabilitySchema, {
          available: false,
          exclusionReasons: [InferenceRouteUnavailabilityReason.NO_ELIGIBLE_TEXT_MEMBER],
        }),
      }),
    ];
    renderAppsSection({
      client: clientFor([catalogApplication], []),
      routeClient: routeClientFor(routes),
      initialEntry: "/apps/catalog",
    });
    await user.click(await screen.findByRole("row", { name: "OpenWebUI" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    const dialog = screen.getByRole("dialog");
    const routeControl = within(dialog).getByRole("combobox", { name: "Inference route" });
    await waitFor(() =>
      expect(
        within(routeControl)
          .getAllByRole("option")
          .map((option) => [option.textContent, (option as HTMLOptionElement).disabled]),
      ).toEqual([
        ["No inference route", false],
        ["chat — Available", false],
        ["disabled — Disabled", false],
        ["vision — Unavailable (No eligible text member)", false],
      ]),
    );
    expect(routeControl).toHaveValue("");
  });

  it("sends the selected stable route ID and exact generation", async () => {
    const user = userEvent.setup();
    const catalogApplication = routeInputApp();
    const client = clientFor([catalogApplication], []);
    client.application.createApplicationDeployment = vi.fn(() =>
      Promise.resolve(
        create(CreateApplicationDeploymentResponseSchema, {
          deployment: create(ApplicationDeploymentSchema, {
            ...deployment(
              catalogApplication,
              ApplicationLifecycleState.QUEUED,
              ApplicationReadinessState.NOT_READY,
              "accepted-deployment",
            ),
            applicationDeploymentName: "Custom • App",
          }),
        }),
      ),
    );
    const routes = [
      create(InferenceRouteViewSchema, {
        requestKinds: [InferenceRequestKind.CHAT],
        route: create(InferenceRouteSchema, {
          inferenceRouteId: "route-id",
          name: "renamable-chat",
          enabled: true,
          generation: 9n,
        }),
        availability: create(InferenceRouteAvailabilitySchema, {
          available: true,
          availableInputModalities: [ModelInputModality.TEXT],
        }),
      }),
    ];
    renderAppsSection({
      client,
      routeClient: routeClientFor(routes),
      initialEntry: "/apps/catalog",
    });
    await user.click(await screen.findByRole("row", { name: "OpenWebUI" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    const dialog = screen.getByRole("dialog");
    const routeControl = within(dialog).getByRole("combobox", { name: "Inference route" });
    await waitFor(() => expect(routeControl).toBeEnabled());
    expect(routeControl).toHaveValue("");
    await user.selectOptions(routeControl, "route-id");
    await openAdvanced(user, within(dialog));
    await user.selectOptions(within(dialog).getByLabelText("Execution target"), "local");
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    await user.clear(screen.getByLabelText("Deployment name"));
    await user.type(screen.getByLabelText("Deployment name"), "  Custom • App  ");
    expect(within(dialog).getByText("renamable-chat")).toBeVisible();
    expect(
      within(dialog)
        .getAllByText("Execution target")
        .map((label) => label.nextElementSibling?.textContent),
    ).toContain("local / Docker applications");
    await user.click(within(dialog).getByRole("button", { name: "Deploy" }));
    expect(client.application.createApplicationDeployment).toHaveBeenCalledWith({
      applicationId: "open-webui",
      applicationDeploymentName: "Custom • App",
      placement: { siteNodeId: "local", executionTargetId: "local" },
      parameterValues: [],
      inferenceRouteId: "route-id",
      expectedInferenceRouteGeneration: 9n,
    });
    expect(await screen.findByRole("complementary", { name: "Custom • App" })).toBeVisible();
  });

  it("offers the streamed routes in the deploy dialog and follows route changes while it is open", async () => {
    const user = userEvent.setup();
    const chat = routeNamed("route-chat", "chat");
    let routes = [chat];
    const routeClient = routeClientFor([]);
    routeClient.route.listRoutes = vi.fn(() =>
      Promise.resolve(create(ListRoutesResponseSchema, { routes })),
    );

    const { watch } = renderAppsSection({
      client: clientFor([routeInputApp()], []),
      routeClient,
      initialEntry: "/apps/catalog",
    });
    await user.click(await screen.findByRole("row", { name: "OpenWebUI" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    const dialog = screen.getByRole("dialog");
    expect(
      within(dialog)
        .getAllByRole("option")
        .map((option) => option.textContent?.split(" — ")[0]),
    ).toEqual(["No inference route", "chat"]);

    routes = [chat, routeNamed("route-docs", "docs")];
    await act(async () => {
      await watch.tick();
    });
    expect(
      within(dialog)
        .getAllByRole("option")
        .map((option) => option.textContent?.split(" — ")[0]),
    ).toEqual(["No inference route", "chat", "docs"]);
  });

  it("keeps the route choice disabled but deploys unlinked while RouteService is loading", async () => {
    const user = userEvent.setup();
    let resolveRoutes!: (
      response: ReturnType<typeof create<typeof ListRoutesResponseSchema>>,
    ) => void;
    const routeClient: RouteClient = {
      route: {
        listRoutes: vi.fn(
          () =>
            new Promise<ReturnType<typeof create<typeof ListRoutesResponseSchema>>>((resolve) => {
              resolveRoutes = resolve;
            }),
        ),
        getRoute: vi.fn(),
        createRoute: vi.fn(),
        replaceRoute: vi.fn(),
        deleteRoute: vi.fn(),
      },
    };
    renderAppsSection({
      client: clientFor([routeInputApp()], []),
      routeClient: routeClient,
      initialEntry: "/apps/catalog",
    });
    await user.click(await screen.findByRole("row", { name: "OpenWebUI" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    const dialog = screen.getByRole("dialog");
    expect(within(dialog).getByRole("combobox", { name: "Inference route" })).toBeDisabled();
    expect(within(dialog).getByText(/route choices are unavailable/)).toBeVisible();
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    await waitFor(() =>
      expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeEnabled(),
    );
    resolveRoutes(create(ListRoutesResponseSchema));
  });

  it("retains choices but fences a linked route after RouteService refresh fails", async () => {
    vi.useFakeTimers();
    let reads = 0;
    const routes = [
      create(InferenceRouteViewSchema, {
        requestKinds: [InferenceRequestKind.CHAT],
        route: create(InferenceRouteSchema, {
          inferenceRouteId: "route-id",
          name: "chat",
          enabled: true,
          generation: 1n,
        }),
        availability: create(InferenceRouteAvailabilitySchema, {
          available: true,
          availableInputModalities: [ModelInputModality.TEXT],
        }),
      }),
    ];
    const routeClient: RouteClient = {
      route: {
        listRoutes: vi.fn(() =>
          ++reads === 1
            ? Promise.resolve(create(ListRoutesResponseSchema, { routes }))
            : Promise.reject(new Error("route manager offline")),
        ),
        getRoute: vi.fn(),
        createRoute: vi.fn(),
        replaceRoute: vi.fn(),
        deleteRoute: vi.fn(),
      },
    };
    renderAppsSection({
      client: clientFor([routeInputApp()], []),
      routeClient: routeClient,
      initialEntry: "/apps/catalog",
    });
    await act(async () => {});
    fireEvent.click(screen.getByRole("row", { name: "OpenWebUI" }));
    fireEvent.click(screen.getByRole("button", { name: "Deploy" }));
    const dialog = screen.getByRole("dialog");
    const routeControl = within(dialog).getByRole("combobox", { name: "Inference route" });
    expect(within(dialog).getByRole("option", { name: /^chat — / })).toBeVisible();
    fireEvent.change(routeControl, { target: { value: "route-id" } });
    expect(routeControl).toHaveValue("route-id");
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
    });
    expect(screen.getByRole("alert", { name: "Inference routes status" })).toHaveTextContent(
      "Inference routes are unavailable",
    );
    expect(within(dialog).getByRole("option", { name: /^chat — / })).toBeVisible();
    expect(routeControl).toBeDisabled();
    expect(within(dialog).getByRole("button", { name: "Next" })).toBeDisabled();
  });

  it("fences duplicate creates while the accepted request is pending", async () => {
    const user = userEvent.setup();
    const catalogApplication = app("open-webui", "OpenWebUI");
    const accepted = deployment(
      catalogApplication,
      ApplicationLifecycleState.QUEUED,
      ApplicationReadinessState.NOT_READY,
      "accepted-deployment",
    );
    let resolveCreate!: (response: ReturnType<typeof createCreateResponse>) => void;
    const createResult = new Promise<ReturnType<typeof createCreateResponse>>((resolve) => {
      resolveCreate = resolve;
    });
    const client = clientFor([catalogApplication], []);
    client.application.createApplicationDeployment = vi.fn(() => createResult);

    renderAppsSection({ client, initialEntry: "/apps/catalog" });
    await user.click(await screen.findByRole("row", { name: "OpenWebUI" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    const dialog = screen.getByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    const deployButton = within(dialog).getByRole("button", { name: "Deploy" });
    await user.click(deployButton);

    expect(client.application.createApplicationDeployment).toHaveBeenCalledTimes(1);
    expect(within(dialog).getByRole("button", { name: "Deploying…" })).toBeDisabled();
    await act(async () => {
      resolveCreate(createCreateResponse(accepted));
      await Promise.resolve();
    });
    expect(await screen.findByRole("complementary", { name: "OpenWebUI" })).toBeVisible();
  });

  it.each([
    [Code.Aborted, "Create outcome is uncertain (Aborted)"],
    [Code.FailedPrecondition, "Create was rejected (FailedPrecondition)"],
    [Code.Unavailable, "Create outcome is uncertain (Unavailable)"],
  ])("presents create error code %s", async (code, expectedMessage) => {
    const user = userEvent.setup();
    const catalogApplication = app("open-webui", "OpenWebUI");
    const client = clientFor([catalogApplication], []);
    client.application.createApplicationDeployment = vi.fn(() =>
      Promise.reject(new ConnectError("controller response", code)),
    );

    renderAppsSection({ client, initialEntry: "/apps/catalog" });
    await user.click(await screen.findByRole("row", { name: "OpenWebUI" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    const dialog = screen.getByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    await user.click(within(dialog).getByRole("button", { name: "Deploy" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(expectedMessage);
    expect(screen.getByRole("alert")).toHaveTextContent("controller response");
  });

  it("lists the catalog applications and none of the installed ones", async () => {
    const catalogApplication = app("open-webui", "OpenWebUI");
    const installed = deployment(
      app("historic", "Historic App"),
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.READY,
    );

    renderAppsSection({
      client: clientFor([catalogApplication], [installed]),
      initialEntry: "/apps/catalog",
    });

    const table = await screen.findByRole("table", { name: "Application catalog" });
    expect(
      within(table)
        .getAllByRole("row")
        .slice(1)
        .map((row) => row.getAttribute("aria-label")),
    ).toEqual(["OpenWebUI"]);
    expect(
      screen.queryByRole("article", { name: "Historic App deployment" }),
    ).not.toBeInTheDocument();
  });

  it("renders an actionable empty state", async () => {
    renderAppsSection({ client: clientFor([], []), initialEntry: "/apps/catalog" });

    expect(await screen.findByText("No applications are available in the catalog.")).toBeVisible();
  });

  it("alerts when catalog loading fails", async () => {
    const client = clientFor([], []);
    client.catalog.listApplications = vi.fn(() => Promise.reject(new Error("catalog offline")));

    renderAppsSection({ client, initialEntry: "/apps/catalog" });

    expect(await screen.findByRole("alert", { name: "Catalog status" })).toHaveTextContent(
      "Catalog is unavailable",
    );
  });

  it("keeps a deep-linked catalog application panel while the deployments are still loading", async () => {
    const client = clientFor([app("open-webui", "OpenWebUI")], []);
    let resolveDeployments!: (response: ListApplicationDeploymentsResponse) => void;
    client.application.listApplicationDeployments = vi.fn(
      () =>
        new Promise<ListApplicationDeploymentsResponse>((resolve) => {
          resolveDeployments = resolve;
        }),
    );

    const { router } = renderAppsSection({
      client,
      initialEntry: "/apps/catalog?panel=app-catalog%2Fcatalog%3Aopen-webui",
    });
    await waitFor(() => expect(client.catalog.listApplications).toHaveBeenCalled());
    await act(async () => {
      await Promise.resolve();
    });
    expect(currentLocation(router)).toBe("/apps/catalog?panel=app-catalog%2Fcatalog%3Aopen-webui");
    act(() => resolveDeployments(create(ListApplicationDeploymentsResponseSchema)));

    const panel = await screen.findByRole("complementary", { name: "OpenWebUI" });
    expect(within(panel).getByRole("region", { name: "Catalog application" })).toBeVisible();
    expect(currentLocation(router)).toBe("/apps/catalog?panel=app-catalog%2Fcatalog%3Aopen-webui");
  });

  it("keeps a deep-linked catalog application panel when the first catalog read fails", async () => {
    const client = clientFor([], []);
    client.catalog.listApplications = vi.fn(() => Promise.reject(new Error("catalog offline")));

    const { router } = renderAppsSection({
      client,
      initialEntry: "/apps/catalog?panel=app-catalog%2Fcatalog%3Aopen-webui",
    });

    expect(await screen.findByRole("alert", { name: "Catalog status" })).toHaveTextContent(
      "Catalog is unavailable",
    );
    expect(currentLocation(router)).toBe("/apps/catalog?panel=app-catalog%2Fcatalog%3Aopen-webui");
  });

  it("inspects a catalog application through its row and URL", async () => {
    const user = userEvent.setup();
    const { router } = renderAppsSection({
      client: clientFor([app("open-webui", "OpenWebUI")], []),
      initialEntry: "/apps/catalog",
    });

    const row = await screen.findByRole("row", { name: "OpenWebUI" });
    await user.click(row);

    expect(currentLocation(router)).toBe("/apps/catalog?panel=app-catalog%2Fcatalog%3Aopen-webui");
    const panel = await screen.findByRole("complementary", { name: "OpenWebUI" });
    expect(within(panel).getByRole("region", { name: "Catalog application" })).toBeVisible();
    expectResourceMarker(within(row).getByText("OpenWebUI"), "open-webui");
    expect(row).toHaveAttribute("data-state", "selected");
  });

  it("drops a stale Catalog link on /apps instead of rewriting it", async () => {
    const { router } = renderAppsSection({
      client: clientFor([app("open-webui", "OpenWebUI")], []),
      initialEntry: "/apps?panel=apps%2Fcatalog%3Aopen-webui",
    });

    await waitFor(() => expect(currentLocation(router)).toBe("/apps"));
    expect(await screen.findByText("No applications deployed")).toBeVisible();
    expect(screen.queryByRole("complementary")).not.toBeInTheDocument();
  });

  it("closes an inspected application back to the bare Catalog", async () => {
    const user = userEvent.setup();
    const { router } = renderAppsSection({
      client: clientFor([app("open-webui", "OpenWebUI")], []),
      initialEntry: "/apps/catalog?panel=app-catalog%2Fcatalog%3Aopen-webui",
    });

    await user.click(await screen.findByRole("button", { name: "Close inspector" }));

    await waitFor(() => expect(currentLocation(router)).toBe("/apps/catalog"));
  });
});

describe("AppCatalogPage deployments", () => {
  const webui = app("open-webui", "OpenWebUI");
  const hermes = app("hermes-app", "Hermes");

  function named(
    deploymentValue: ReturnType<typeof deployment>,
    applicationDeploymentName: string,
    desiredState = ApplicationDeploymentDesiredState.PRESENT,
  ) {
    return create(ApplicationDeploymentSchema, {
      ...deploymentValue,
      applicationDeploymentName,
      desiredState,
    });
  }

  function renderWebuiDeployments() {
    const ready = ApplicationReadinessState.READY;
    return renderAppsSection({
      client: clientFor(
        [webui, hermes, app("idle-app", "Idle")],
        [
          named(deployment(webui, ApplicationLifecycleState.READY, ready, "webui-a"), "Team chat"),
          named(
            deployment(webui, ApplicationLifecycleState.STARTING, ready, "webui-b"),
            "Lab chat",
          ),
          named(
            deployment(webui, ApplicationLifecycleState.READY, ready, "webui-old"),
            "Old chat",
            ApplicationDeploymentDesiredState.TEARDOWN,
          ),
          named(deployment(hermes, ApplicationLifecycleState.READY, ready, "hermes-a"), "Agent"),
        ],
      ),
      initialEntry: "/apps/catalog",
    });
  }

  it("lists the catalog application's deployments with their placement and status", async () => {
    const user = userEvent.setup();
    renderWebuiDeployments();

    const row = await screen.findByRole("row", { name: "OpenWebUI" });
    await waitFor(() => expect(within(row).getByText("2 deployed")).toBeVisible());
    await user.click(row);
    const section = await screen.findByRole("region", { name: "Deployments" });

    expect(within(section).getByText("2 deployments")).toBeVisible();
    const list = within(section).getByRole("list", { name: "Deployments of OpenWebUI" });
    expect(
      within(list)
        .getAllByRole("button")
        .map((button) => button.getAttribute("aria-label")),
    ).toEqual(["View deployment Team chat, Ready", "View deployment Lab chat, Starting"]);
    expect(within(list).getAllByText("local / Docker applications")).toHaveLength(2);
  });

  it("opens a listed deployment above the catalog application, and Back returns to it", async () => {
    const user = userEvent.setup();
    const { router } = renderWebuiDeployments();

    await user.click(await screen.findByRole("row", { name: "OpenWebUI" }));
    const section = await screen.findByRole("region", { name: "Deployments" });
    await user.click(
      within(section).getByRole("button", { name: "View deployment Team chat, Ready" }),
    );

    await waitFor(() =>
      expect(currentLocation(router)).toBe(
        "/apps?panel=app-catalog%2Fcatalog%3Aopen-webui&panel=apps%2Fdeployment%3Awebui-a",
      ),
    );
    expect(await screen.findByRole("complementary", { name: "Team chat" })).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Back to OpenWebUI" }));

    await waitFor(() =>
      expect(currentLocation(router)).toBe(
        "/apps/catalog?panel=app-catalog%2Fcatalog%3Aopen-webui",
      ),
    );
    expect(await screen.findByRole("region", { name: "Catalog application" })).toBeVisible();
  });

  it("shows no deployments section for an undeployed catalog application", async () => {
    const user = userEvent.setup();
    renderWebuiDeployments();

    await user.click(await screen.findByRole("row", { name: "Idle" }));

    expect(await screen.findByRole("region", { name: "Catalog application" })).toBeVisible();
    expect(screen.queryByRole("region", { name: "Deployments" })).not.toBeInTheDocument();
  });

  it("shows the deployments as unreachable when the site's deployments cannot be read", async () => {
    const user = userEvent.setup();
    const client = clientFor([webui], []);
    client.application.listApplicationDeployments = vi.fn(() =>
      Promise.reject(new Error("deployments offline")),
    );
    renderAppsSection({ client, initialEntry: "/apps/catalog" });

    await user.click(await screen.findByRole("row", { name: "OpenWebUI" }));
    const section = await screen.findByRole("region", { name: "Deployments" });

    expect(within(section).getByText("Unreachable")).toBeVisible();
    expect(within(section).queryByRole("list")).not.toBeInTheDocument();
  });
});

describe("Deployment to catalog application link", () => {
  const application = app("open-webui", "OpenWebUI");
  const installed = deployment(
    application,
    ApplicationLifecycleState.READY,
    ApplicationReadinessState.READY,
    "deployment-1",
  );

  it("opens the Catalog panel above the deployment, and its back label returns to the deployment", async () => {
    const user = userEvent.setup();
    const { router } = renderAppsSection({
      client: clientFor([application], [installed]),
      initialEntry: "/apps?panel=apps%2Fdeployment%3Adeployment-1",
    });

    const section = await screen.findByRole("region", { name: "Application" });
    await user.click(within(section).getByRole("link", { name: "OpenWebUI" }));

    await waitFor(() =>
      expect(currentLocation(router)).toBe(
        "/apps/catalog?panel=apps%2Fdeployment%3Adeployment-1&panel=app-catalog%2Fcatalog%3Aopen-webui",
      ),
    );
    expect(await screen.findByRole("region", { name: "Catalog application" })).toBeVisible();

    await user.click(await screen.findByRole("button", { name: "Back to OpenWebUI" }));

    await waitFor(() =>
      expect(currentLocation(router)).toBe("/apps?panel=apps%2Fdeployment%3Adeployment-1"),
    );
    expect(await screen.findByRole("region", { name: "Application" })).toBeVisible();
  });

  it("returns to the deployment from another catalog application opened from the table", async () => {
    const user = userEvent.setup();
    const { router } = renderAppsSection({
      client: clientFor([application, app("hermes-app", "Hermes")], [installed]),
      initialEntry: "/apps?panel=apps%2Fdeployment%3Adeployment-1",
    });

    const section = await screen.findByRole("region", { name: "Application" });
    await user.click(within(section).getByRole("link", { name: "OpenWebUI" }));
    await user.click(await screen.findByRole("row", { name: "Hermes" }));
    await waitFor(() =>
      expect(currentLocation(router)).toBe(
        "/apps/catalog?panel=apps%2Fdeployment%3Adeployment-1&panel=app-catalog%2Fcatalog%3Ahermes-app",
      ),
    );
    await screen.findByRole("complementary", { name: "Hermes" });
    await user.click(screen.getByRole("button", { name: "Back to OpenWebUI" }));

    await waitFor(() =>
      expect(currentLocation(router)).toBe("/apps?panel=apps%2Fdeployment%3Adeployment-1"),
    );
    expect(await screen.findByRole("region", { name: "Application" })).toBeVisible();
  });

  it("returns to the deployment the catalog application was opened from when it is selected in the list", async () => {
    const user = userEvent.setup();
    const { router } = renderAppsSection({
      client: clientFor([application], [installed]),
      initialEntry: "/apps?panel=apps%2Fdeployment%3Adeployment-1",
    });

    const section = await screen.findByRole("region", { name: "Application" });
    await user.click(within(section).getByRole("link", { name: "OpenWebUI" }));
    const deployments = await screen.findByRole("region", { name: "Deployments" });
    await user.click(
      within(deployments).getByRole("button", { name: "View deployment OpenWebUI, Ready" }),
    );

    await waitFor(() =>
      expect(currentLocation(router)).toBe("/apps?panel=apps%2Fdeployment%3Adeployment-1"),
    );
    expect(await screen.findByRole("region", { name: "Application" })).toBeVisible();
  });

  it("states the application as plain text when it is not in the catalog", async () => {
    renderAppsSection({
      client: clientFor([], [installed]),
      initialEntry: "/apps?panel=apps%2Fdeployment%3Adeployment-1",
    });

    const section = await screen.findByRole("region", { name: "Application" });
    expect(within(section).getAllByText("OpenWebUI").length).toBeGreaterThan(0);
    expect(within(section).queryByRole("link")).not.toBeInTheDocument();
  });
});

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((resolvePromise) => {
    resolve = resolvePromise;
  });

  return { promise, resolve };
}

function routeNamed(inferenceRouteId: string, name: string) {
  return create(InferenceRouteViewSchema, {
    requestKinds: [InferenceRequestKind.CHAT],
    route: create(InferenceRouteSchema, { inferenceRouteId, name, enabled: true, generation: 1n }),
    availability: create(InferenceRouteAvailabilitySchema, {
      available: true,
      availableInputModalities: [ModelInputModality.TEXT],
    }),
  });
}
