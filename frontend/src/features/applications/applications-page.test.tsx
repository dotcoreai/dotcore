import { create } from "@bufbuild/protobuf";
import { timestampFromDate } from "@bufbuild/protobuf/wkt";
import { Code, ConnectError } from "@connectrpc/connect";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { ApplicationClient } from "@/api/application-client";
import type { DeploymentLogOperations } from "@/api/deployment-log-client";
import type { MetricsClient } from "@/api/metrics-client";
import {
  gaugePoint,
  metricsClientWith,
  metricSeries,
  queryResult,
  staticWatch,
} from "@/features/metrics/metric-test-support";
import {
  type ApplicationDeployment,
  ApplicationDeploymentDesiredState,
  ApplicationDeploymentSchema,
  ApplicationSelectedInferenceRouteSchema,
  ApplicationServiceAcceleratorMemorySchema,
} from "@/gen/dotcore/application/v1/deployment_pb";
import {
  type ListApplicationDeploymentsResponse,
  ListApplicationDeploymentsResponseSchema,
  PauseDeploymentResponseSchema,
  RemoveApplicationDeploymentResponseSchema,
  ResumeDeploymentResponseSchema,
} from "@/gen/dotcore/application/v1/service_pb";
import {
  ApplicationDeploymentStateSchema,
  ApplicationLifecycleState,
  ApplicationReadinessState,
} from "@/gen/dotcore/application/v1/state_pb";
import { ListApplicationsResponseSchema } from "@/gen/dotcore/catalog/v1/service_pb";
import { ExactPlacementSchema } from "@/gen/dotcore/deployment/v1/placement_pb";
import {
  ListConfiguredExecutionTargetsResponseSchema,
  NodeObservationState,
} from "@/gen/dotcore/site/v1/service_pb";
import { LOADING_MINIMUM_VISIBLE_MS, LOADING_REVEAL_DELAY_MS } from "@/hooks/use-delayed-loading";
import { expectHeadingOutline } from "@/test/heading-outline";
import { chooseMoreAction, inspectorActionList, pickMoreAction } from "@/test/inspector-actions";
import { expectResourceMarker, inspectorTitle } from "@/test/resource-marker";

import {
  app,
  applicationTarget,
  clientFor,
  currentLocation,
  deployment,
  renderAppsSection,
  routeInputApp,
} from "./applications-section-test-support";

afterEach(() => {
  vi.useRealTimers();
});

describe("ApplicationsPage", () => {
  it("links the deployment's machine and route to their inspectors", async () => {
    const application = routeInputApp();
    const linked = deployment(
      application,
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.READY,
    );
    linked.applicationDeploymentName = "Team chat";
    linked.placement = create(ExactPlacementSchema, {
      siteNodeId: "worker",
      executionTargetId: "remote-app",
    });
    linked.selectedInferenceRoute = create(ApplicationSelectedInferenceRouteSchema, {
      inferenceRouteId: "route-1",
      routeName: "private-chat",
    });
    const client = clientFor([application], [linked]);
    client.site.listConfiguredExecutionTargets = vi.fn(() =>
      Promise.resolve(
        create(ListConfiguredExecutionTargetsResponseSchema, {
          executionTargets: [
            applicationTarget("local", "local"),
            applicationTarget("worker", "remote-app", {
              siteManager: false,
              siteNodeDisplayName: "gb10-host-2",
            }),
          ],
        }),
      ),
    );
    renderAppsSection({
      client,
      machines: [{ displayName: "gb10-host-2", machineId: "worker" }],
      initialEntry: "/apps?panel=apps%2Fdeployment%3Aopen-webui-deployment",
    });

    const deploymentPanel = await screen.findByRole("complementary", { name: "Team chat" });
    expect(
      await within(deploymentPanel).findByRole("link", {
        name: "gb10-host-2 / Docker applications",
      }),
    ).toHaveAttribute(
      "href",
      "/machines?panel=apps%2Fdeployment%3Aopen-webui-deployment&panel=machines%2Fmachine%3Aworker",
    );
    expect(within(deploymentPanel).getByRole("link", { name: "private-chat" })).toHaveAttribute(
      "href",
      "/routes?panel=apps%2Fdeployment%3Aopen-webui-deployment&panel=routes%2Froute%3Aroute-1",
    );
    expect(within(deploymentPanel).getByRole("link", { name: "OpenWebUI" })).toHaveAttribute(
      "href",
      "/apps/catalog?panel=apps%2Fdeployment%3Aopen-webui-deployment&panel=app-catalog%2Fcatalog%3Aopen-webui",
    );
  });

  it("offers Deploy as links to the Apps Catalog in the header and empty state", async () => {
    renderAppsSection({ client: clientFor([], []) });

    await waitFor(() =>
      expect(
        screen
          .getAllByRole("link", { name: "Deploy" })
          .map((link) => [link.getAttribute("href"), link.querySelector("svg")?.classList[1]]),
      ).toEqual([
        ["/apps/catalog", "lucide-rocket"],
        ["/apps/catalog", "lucide-rocket"],
      ]),
    );
  });

  it("reads h1 page and h2 section with no skipped level", async () => {
    const application = app("open-webui", "OpenWebUI");
    renderAppsSection({
      client: clientFor(
        [application],
        [deployment(application, ApplicationLifecycleState.READY, ApplicationReadinessState.READY)],
      ),
    });
    await waitFor(() => {
      expect(screen.getAllByRole("heading", { level: 2 }).map((h) => h.textContent)).toEqual([
        "Installed",
      ]);
      expectHeadingOutline(document.body, [1, 2]);
    });
  });

  it("delays initial feedback and holds completed Apps content for the minimum", async () => {
    vi.useFakeTimers();
    let resolveCatalog!: (
      response: Awaited<ReturnType<ApplicationClient["catalog"]["listApplications"]>>,
    ) => void;
    const client = clientFor([], []);
    client.catalog.listApplications = vi.fn(
      () =>
        new Promise<Awaited<ReturnType<ApplicationClient["catalog"]["listApplications"]>>>(
          (resolve) => {
            resolveCatalog = resolve;
          },
        ),
    );

    renderAppsSection({ client });

    expect(screen.queryByText("Loading apps…")).not.toBeInTheDocument();
    expect(client.catalog.listApplications).toHaveBeenCalledTimes(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(LOADING_REVEAL_DELAY_MS);
    });
    expect(client.site.listConfiguredExecutionTargets).toHaveBeenCalledTimes(1);
    expect(client.application.listApplicationDeployments).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("status")).toHaveTextContent("Loading apps…");

    await act(async () => {
      resolveCatalog(create(ListApplicationsResponseSchema));
      await Promise.resolve();
    });
    expect(screen.getByRole("status")).toHaveTextContent("Loading apps…");
    expect(screen.queryByText("No applications deployed")).not.toBeInTheDocument();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(LOADING_MINIMUM_VISIBLE_MS - 1);
    });
    expect(screen.getByRole("status")).toHaveTextContent("Loading apps…");

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1);
    });
    expect(screen.queryByText("Loading apps…")).not.toBeInTheDocument();
    expect(screen.getByText("No applications deployed")).toBeVisible();
  });
  it("restores an application deployment and its log modal from a direct log URL", async () => {
    const user = userEvent.setup();
    const application = app("open-webui", "OpenWebUI");
    const client = clientFor(
      [application],
      [
        deployment(
          application,
          ApplicationLifecycleState.READY,
          ApplicationReadinessState.READY,
          "deployment-logs",
        ),
      ],
    );
    const read: DeploymentLogOperations["read"] = vi.fn().mockResolvedValue({
      entries: [],
      historyState: "no-older-output-observed",
      latestPositions: [],
      responseTruncated: false,
      sources: [],
    });
    const logs: DeploymentLogOperations = {
      follow: vi.fn(async function* () {}),
      read,
    };
    client.logs = logs;

    renderAppsSection({
      client: client,
      initialEntry: "/apps?panel=apps%2Flogs%3Adeployment-logs",
    });

    const logViewer = await screen.findByRole("dialog", { name: "OpenWebUI · Logs" });
    expect(logViewer).toBeVisible();
    expectResourceMarker(inspectorTitle(logViewer), "deployment-logs");
    expect(screen.getByTestId("inspector-content")).toBeInTheDocument();
    // The deployment sidebar beneath the log window is a first panel, with nothing to go back to.
    expect(
      screen.queryByRole("button", { hidden: true, name: /^Back to/ }),
    ).not.toBeInTheDocument();
    expect(read).toHaveBeenCalledWith(
      {
        beforeTimestamp: undefined,
        deploymentId: "deployment-logs",
        pageSize: 200,
        sourceNames: undefined,
      },
      expect.any(AbortSignal),
    );

    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(screen.getByRole("button", { name: "Logs" })).toHaveFocus();
  });

  it("does not expose direct Models or serving scopes on the Apps page", async () => {
    renderAppsSection({ client: clientFor([], []) });
    expect(await screen.findByText("No applications deployed")).toBeVisible();
    expect(screen.queryByText(/Serving Models|Serving scope|model-a/)).not.toBeInTheDocument();
  });

  it("renders exactly one projected status badge per deployment row", async () => {
    const application = app("open-webui", "OpenWebUI");
    const client = clientFor(
      [],
      [
        deployment(
          application,
          ApplicationLifecycleState.QUEUED,
          ApplicationReadinessState.NOT_READY,
        ),
      ],
    );

    renderAppsSection({ client });

    const row = await screen.findByRole("row", { name: "OpenWebUI" });
    expect(row.querySelectorAll('[data-slot="badge"]')).toHaveLength(1);
    expect(within(row).getByText("Queued")).toBeVisible();
    // Lifecycle and readiness are no longer joined into one badge string.
    expect(row).not.toHaveTextContent(" · Not ready");
    // The readiness detail qualifies the badge, so it rides its tooltip rather than the row.
    expect(within(row).queryByText("Waiting for endpoint")).not.toBeInTheDocument();
    expect(row.querySelector('[data-slot="badge"]')).toHaveAttribute(
      "title",
      "Waiting for endpoint",
    );
  });

  it("removes by exact ID, stays visible while tearing down, then closes after deletion", async () => {
    vi.useFakeTimers();
    const application = app("open-webui", "OpenWebUI");
    const present = deployment(
      application,
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.READY,
      "remove-me",
    );
    const stopping = create(ApplicationDeploymentSchema, {
      ...present,
      desiredState: ApplicationDeploymentDesiredState.TEARDOWN,
      state: create(ApplicationDeploymentStateSchema, {
        lifecycleState: ApplicationLifecycleState.STOPPING,
        readinessState: ApplicationReadinessState.NOT_READY,
      }),
    });
    const client = clientFor([], [present]);
    let removed = false;
    client.application.listApplicationDeployments = vi.fn(() =>
      Promise.resolve(
        create(ListApplicationDeploymentsResponseSchema, { deployments: removed ? [] : [present] }),
      ),
    );
    client.application.removeApplicationDeployment = vi.fn(() => {
      removed = true;
      return Promise.resolve(
        create(RemoveApplicationDeploymentResponseSchema, { deployment: stopping }),
      );
    });

    renderAppsSection({ client });
    await act(async () => {});
    fireEvent.click(screen.getByRole("row", { name: "OpenWebUI" }));
    pickMoreAction("Teardown…");
    fireEvent.click(screen.getByRole("button", { name: "Teardown" }));
    await act(async () => {});

    expect(client.application.removeApplicationDeployment).toHaveBeenCalledTimes(1);
    expect(client.application.removeApplicationDeployment).toHaveBeenCalledWith({
      applicationDeploymentId: "remove-me",
    });
    // Row and open panel both state it
    expect(screen.getAllByText("Tearing down")).toHaveLength(2);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
      await Promise.resolve();
    });
    expect(screen.queryByRole("row", { name: "OpenWebUI" })).not.toBeInTheDocument();
    expect(screen.queryByRole("complementary", { name: "open-webui-1" })).not.toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Apps" })).toHaveFocus();
  });

  it("does not call remove when cancellation uses Cancel or Escape", async () => {
    const user = userEvent.setup();
    const application = app("open-webui", "OpenWebUI");
    const client = clientFor(
      [],
      [deployment(application, ApplicationLifecycleState.READY, ApplicationReadinessState.READY)],
    );

    renderAppsSection({ client });
    await user.click(await screen.findByRole("row", { name: "OpenWebUI" }));
    await chooseMoreAction(user, "Teardown…");
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    await chooseMoreAction(user, "Teardown…");
    await user.keyboard("{Escape}");

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(client.application.removeApplicationDeployment).not.toHaveBeenCalled();
  });

  it("applies an accepted Pause immediately, disables conflicts, suppresses endpoint access, and restores row focus", async () => {
    const user = userEvent.setup();
    const application = app("pause-app", "Pause App");
    const ready = deployment(
      application,
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.READY,
      "pause-deployment",
    );
    const accepted = create(ApplicationDeploymentSchema, {
      ...ready,
      desiredState: ApplicationDeploymentDesiredState.PAUSED,
      state: create(ApplicationDeploymentStateSchema, {
        endpoint: { url: "http://100.64.0.7:49152" },
        lifecycleState: ApplicationLifecycleState.PAUSING,
        readinessState: ApplicationReadinessState.READY,
      }),
    });
    const client = clientFor([], [ready]);
    let deploymentReads = 0;
    client.application.listApplicationDeployments = vi.fn(() => {
      deploymentReads += 1;
      return deploymentReads === 1
        ? Promise.resolve(
            create(ListApplicationDeploymentsResponseSchema, { deployments: [ready] }),
          )
        : new Promise<ListApplicationDeploymentsResponse>(() => undefined);
    });
    client.application.pauseDeployment = vi.fn(() =>
      Promise.resolve(create(PauseDeploymentResponseSchema, { deployment: accepted })),
    );

    renderAppsSection({ client });
    const opener = await screen.findByRole("row", { name: "Pause App" });
    await user.click(opener);
    await chooseMoreAction(user, "Pause");

    expect(client.application.pauseDeployment).toHaveBeenCalledWith({
      applicationDeploymentId: "pause-deployment",
    });
    expect(screen.getAllByText("Pausing")[0]).toBeVisible();
    expect(await inspectorActionList(user)).toEqual({
      buttons: ["Resume (disabled)", "More actions"],
      menu: ["Edit", "Teardown… (disabled)"],
    });
    expect(screen.queryByRole("link", { name: "Open" })).not.toBeInTheDocument();
    const row = screen.getByRole("row", { name: "Pause App" });
    expect(within(row).queryByRole("link", { name: /^Open/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Close inspector" }));
    expect(opener).toHaveFocus();
  });

  it.each([
    ["queued", ApplicationLifecycleState.QUEUED],
    ["preparing", ApplicationLifecycleState.PREPARING],
    ["starting", ApplicationLifecycleState.STARTING],
    ["failed", ApplicationLifecycleState.FAILED],
    ["unavailable", ApplicationLifecycleState.UNAVAILABLE],
  ] as const)(
    "accepts Pause from the %s state without exposing a stale endpoint",
    async (_label, lifecycleState) => {
      const user = userEvent.setup();
      const application = app(`pause-${_label}`, `Pause ${_label}`);
      const baseline = deployment(
        application,
        lifecycleState,
        ApplicationReadinessState.NOT_READY,
        `pause-${_label}-deployment`,
      );
      const accepted = create(ApplicationDeploymentSchema, {
        ...baseline,
        desiredState: ApplicationDeploymentDesiredState.PAUSED,
        state: create(ApplicationDeploymentStateSchema, {
          endpoint: { url: "http://100.64.0.7:49152" },
          lifecycleState: ApplicationLifecycleState.PAUSING,
          readinessState: ApplicationReadinessState.READY,
        }),
      });
      const client = clientFor([], [baseline]);
      let deploymentReads = 0;
      client.application.listApplicationDeployments = vi.fn(() => {
        deploymentReads += 1;
        return deploymentReads === 1
          ? Promise.resolve(
              create(ListApplicationDeploymentsResponseSchema, { deployments: [baseline] }),
            )
          : new Promise<ListApplicationDeploymentsResponse>(() => undefined);
      });
      client.application.pauseDeployment = vi.fn(() =>
        Promise.resolve(createPauseResponse(accepted)),
      );

      renderAppsSection({ client });
      const opener = await screen.findByRole("row", { name: `Pause ${_label}` });
      await user.click(opener);
      await chooseMoreAction(user, "Pause");

      expect(client.application.pauseDeployment).toHaveBeenCalledWith({
        applicationDeploymentId: `pause-${_label}-deployment`,
      });
      expect(screen.getAllByText("Pausing")[0]).toBeVisible();
      expect(await inspectorActionList(user)).toEqual({
        buttons: ["Resume (disabled)", "More actions"],
        menu: ["Edit", "Teardown… (disabled)"],
      });
      expect(screen.queryByRole("link", { name: "Open" })).not.toBeInTheDocument();
      expect(screen.queryByText("http://100.64.0.7:49152")).not.toBeInTheDocument();
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    },
  );

  it("applies an accepted Resume immediately in a startup state and keeps Remove separate", async () => {
    const user = userEvent.setup();
    const application = app("resume-app", "Resume App");
    const paused = create(ApplicationDeploymentSchema, {
      ...deployment(
        application,
        ApplicationLifecycleState.PAUSED,
        ApplicationReadinessState.NOT_READY,
        "resume-deployment",
      ),
      desiredState: ApplicationDeploymentDesiredState.PAUSED,
    });
    const accepted = create(ApplicationDeploymentSchema, {
      ...paused,
      desiredState: ApplicationDeploymentDesiredState.PRESENT,
      state: create(ApplicationDeploymentStateSchema, {
        lifecycleState: ApplicationLifecycleState.STARTING,
        readinessState: ApplicationReadinessState.NOT_READY,
      }),
    });
    const client = clientFor([], [paused]);
    let deploymentReads = 0;
    client.application.listApplicationDeployments = vi.fn(() => {
      deploymentReads += 1;
      return deploymentReads === 1
        ? Promise.resolve(
            create(ListApplicationDeploymentsResponseSchema, { deployments: [paused] }),
          )
        : new Promise<ListApplicationDeploymentsResponse>(() => undefined);
    });
    client.application.resumeDeployment = vi.fn(() =>
      Promise.resolve(create(ResumeDeploymentResponseSchema, { deployment: accepted })),
    );

    renderAppsSection({ client });
    await user.click(await screen.findByRole("row", { name: "Resume App" }));
    await user.click(screen.getByRole("button", { name: "Resume" }));

    expect(client.application.resumeDeployment).toHaveBeenCalledWith({
      applicationDeploymentId: "resume-deployment",
    });
    expect(screen.getAllByText("Starting")[0]).toBeVisible();
    expect(await inspectorActionList(user)).toEqual({
      buttons: ["Edit", "More actions"],
      menu: ["Pause", "Teardown… (disabled)"],
    });
  });

  it.each([
    ["Pause", Code.FailedPrecondition, "pause was rejected", "Pause was rejected"],
    ["Resume", Code.Unavailable, "resume write is uncertain", "Resume outcome is uncertain"],
  ])("reports exact accessible %s errors", async (action, code, detail, message) => {
    const user = userEvent.setup();
    const application = app(`${action.toLowerCase()}-error-app`, `${action} Error App`);
    const desiredState =
      action === "Pause"
        ? ApplicationDeploymentDesiredState.PRESENT
        : ApplicationDeploymentDesiredState.PAUSED;
    const lifecycleState =
      action === "Pause" ? ApplicationLifecycleState.READY : ApplicationLifecycleState.PAUSED;
    const current = create(ApplicationDeploymentSchema, {
      ...deployment(
        application,
        lifecycleState,
        ApplicationReadinessState.READY,
        "error-deployment",
      ),
      desiredState,
    });
    const client = clientFor([], [current]);
    const mutation = vi.fn(() => Promise.reject(new ConnectError(detail, code)));
    if (action === "Pause") {
      client.application.pauseDeployment = mutation;
    } else {
      client.application.resumeDeployment = mutation;
    }

    renderAppsSection({ client });
    await user.click(await screen.findByRole("row", { name: `${action} Error App` }));
    await (action === "Pause"
      ? chooseMoreAction(user, "Pause")
      : user.click(screen.getByRole("button", { name: action })));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      action === "Pause"
        ? `${message} (${Code[code]}): ${detail}. Check the request and service logs.`
        : `${message} (${Code[code]}): ${detail}. Check the Installed section and service logs before retrying.`,
    );
  });

  it.each([
    [
      "missing",
      createRemoveResponse(),
      "Teardown response protocol error: missing or mismatched accepted deployment ID.",
    ],
    [
      "mismatched ID",
      createRemoveResponse(
        create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "other-deployment",
          desiredState: ApplicationDeploymentDesiredState.TEARDOWN,
        }),
      ),
      "Teardown response protocol error: missing or mismatched accepted deployment ID.",
    ],
    [
      "non-TEARDOWN",
      createRemoveResponse(
        deployment(
          app("open-webui", "OpenWebUI"),
          ApplicationLifecycleState.READY,
          ApplicationReadinessState.READY,
        ),
      ),
      "Teardown response protocol error: accepted deployment is not TEARDOWN.",
    ],
  ])("rejects %s removal response without false success", async (_case, response, message) => {
    const user = userEvent.setup();
    const application = app("open-webui", "OpenWebUI");
    const present = deployment(
      application,
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.READY,
    );
    const client = clientFor([], [present]);
    client.application.removeApplicationDeployment = vi.fn(() => Promise.resolve(response));

    renderAppsSection({ client });
    await user.click(await screen.findByRole("row", { name: "OpenWebUI" }));
    await chooseMoreAction(user, "Teardown…");
    await user.click(screen.getByRole("button", { name: "Teardown" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(message);
    expect(
      within(screen.getByRole("table", { name: "Installed applications" })).getByText("Ready"),
    ).toBeVisible();
  });

  it.each([
    [
      Code.FailedPrecondition,
      "Teardown was rejected (FailedPrecondition): controller failed. Check the request and service logs.",
    ],
    [
      Code.Unavailable,
      "Teardown outcome is uncertain (Unavailable): controller failed. Check the Installed section and service logs before retrying.",
    ],
  ])(
    "distinguishes removal failure code %s and keeps the listed deployment",
    async (code, message) => {
      const user = userEvent.setup();
      const application = app("open-webui", "OpenWebUI");
      const present = deployment(
        application,
        ApplicationLifecycleState.READY,
        ApplicationReadinessState.READY,
      );
      const client = clientFor([], [present]);
      client.application.removeApplicationDeployment = vi.fn(() =>
        Promise.reject(new ConnectError("controller failed", code)),
      );

      renderAppsSection({ client });
      await user.click(await screen.findByRole("row", { name: "OpenWebUI" }));
      await chooseMoreAction(user, "Teardown…");
      await user.click(screen.getByRole("button", { name: "Teardown" }));

      expect(await screen.findByRole("alert")).toHaveTextContent(message);
      expect(
        within(screen.getByRole("table", { name: "Installed applications" })).getByText("Ready"),
      ).toBeVisible();
    },
  );

  it("prevents duplicate removal while the accepted request is pending", async () => {
    const user = userEvent.setup();
    const application = app("open-webui", "OpenWebUI");
    const present = deployment(
      application,
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.READY,
    );
    let resolveRemove!: (response: ReturnType<typeof createRemoveResponse>) => void;
    const client = clientFor([], [present]);
    client.application.removeApplicationDeployment = vi.fn(
      () =>
        new Promise<ReturnType<typeof createRemoveResponse>>(
          (resolve) => (resolveRemove = resolve),
        ),
    );

    renderAppsSection({ client });
    await user.click(await screen.findByRole("row", { name: "OpenWebUI" }));
    await chooseMoreAction(user, "Teardown…");
    await user.click(screen.getByRole("button", { name: "Teardown" }));
    expect(client.application.removeApplicationDeployment).toHaveBeenCalledTimes(1);
    const more = screen.getByRole("button", { name: "More actions" });
    expect(more).toBeDisabled();
    expect(more).toHaveAttribute("aria-busy", "true");
    resolveRemove(createRemoveResponse(present));
  });

  it("keeps the accepted teardown overlay over a stale poll", async () => {
    const user = userEvent.setup();
    const application = app("open-webui", "OpenWebUI");
    const present = deployment(
      application,
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.READY,
    );
    const stopping = create(ApplicationDeploymentSchema, {
      ...present,
      desiredState: ApplicationDeploymentDesiredState.TEARDOWN,
      state: create(ApplicationDeploymentStateSchema, {
        lifecycleState: ApplicationLifecycleState.STOPPING,
        readinessState: ApplicationReadinessState.NOT_READY,
      }),
    });
    let resolveStale!: (response: ListApplicationDeploymentsResponse) => void;
    const staleRead = new Promise<ListApplicationDeploymentsResponse>(
      (resolve) => (resolveStale = resolve),
    );
    const client = clientFor([], [present]);
    let reads = 0;
    client.application.listApplicationDeployments = vi.fn(() => {
      reads += 1;
      return reads === 1
        ? Promise.resolve(
            create(ListApplicationDeploymentsResponseSchema, { deployments: [present] }),
          )
        : staleRead;
    });
    client.application.removeApplicationDeployment = vi.fn(() =>
      Promise.resolve(create(RemoveApplicationDeploymentResponseSchema, { deployment: stopping })),
    );

    renderAppsSection({ client });
    await user.click(await screen.findByRole("row", { name: "OpenWebUI" }));
    await chooseMoreAction(user, "Teardown…");
    await user.click(screen.getByRole("button", { name: "Teardown" }));
    await act(async () => {});
    resolveStale(create(ListApplicationDeploymentsResponseSchema, { deployments: [present] }));
    await act(async () => {});

    expect(screen.getAllByText("Tearing down")).toHaveLength(2);
  });

  it("lists only installed applications and reads all controller sources", async () => {
    const catalogApplication = app("open-webui", "OpenWebUI");
    const installedApplication = app("historic", "Historic App");
    const client = clientFor(
      [catalogApplication],
      [
        deployment(
          installedApplication,
          ApplicationLifecycleState.READY,
          ApplicationReadinessState.READY,
        ),
      ],
    );

    renderAppsSection({ client });

    expect(await screen.findByRole("row", { name: "Historic App" })).toBeVisible();
    const heading = screen.getByRole("heading", { level: 2, name: "Installed" });
    expect(heading).toHaveAttribute("data-slot", "page-section-heading");
    expect(screen.queryByRole("heading", { name: "Catalog" })).not.toBeInTheDocument();
    expect(screen.queryByRole("row", { name: "OpenWebUI" })).not.toBeInTheDocument();
    expect(client.catalog.listApplications).toHaveBeenCalledWith({}, expect.anything());
    expect(client.site.listConfiguredExecutionTargets).toHaveBeenCalledWith({});
    expect(client.application.listApplicationDeployments).toHaveBeenCalledWith({});
  });

  it("opens an assigned endpoint without waiting for readiness", async () => {
    const user = userEvent.setup();
    const application = app("open-webui", "OpenWebUI");
    const assigned = create(ApplicationDeploymentSchema, {
      ...deployment(
        application,
        ApplicationLifecycleState.RUNNING,
        ApplicationReadinessState.NOT_READY,
      ),
      state: create(ApplicationDeploymentStateSchema, {
        endpoint: { url: "http://100.64.0.7:49152" },
        lifecycleState: ApplicationLifecycleState.RUNNING,
        readinessState: ApplicationReadinessState.NOT_READY,
      }),
    });
    const client = clientFor([], [assigned]);

    renderAppsSection({ client });
    await user.click(await screen.findByRole("row", { name: "OpenWebUI" }));

    const open = screen.getByRole("link", { name: "Open" });
    expect(open).toBeEnabled();
    expect(open).toHaveAttribute("href", "http://100.64.0.7:49152");
    expect(
      within(screen.getByRole("table", { name: "Installed applications" })).getByText("Not ready"),
    ).toBeVisible();
  });

  it("names a stale machine's machine and last report from the live deployments", async () => {
    vi.useFakeTimers({ now: new Date("2026-10-01T12:00:00Z") });
    const application = app("open-webui", "OpenWebUI");
    const reported = deployment(
      application,
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.READY,
    );
    reported.placement = create(ExactPlacementSchema, {
      siteNodeId: "worker",
      executionTargetId: "remote-app",
    });
    const stale = create(ApplicationDeploymentSchema, {
      ...reported,
      state: create(ApplicationDeploymentStateSchema, {
        lifecycleState: ApplicationLifecycleState.READY,
        observationStale: true,
        readinessState: ApplicationReadinessState.READY,
      }),
    });
    const client = clientFor([], []);
    const responses = [stale, stale];
    const listApplicationDeployments = vi.fn(() =>
      Promise.resolve(
        create(ListApplicationDeploymentsResponseSchema, {
          deployments: [responses.shift() ?? reported],
        }),
      ),
    );
    client.application.listApplicationDeployments = listApplicationDeployments;
    const machines = [
      {
        displayName: "gb10-host-2",
        machineId: "worker",
        observationFreshness: {
          state: NodeObservationState.DISCONNECTED,
          receivedAt: timestampFromDate(new Date("2026-10-01T11:58:00Z")),
        },
      },
    ];
    client.site.listConfiguredExecutionTargets = vi.fn(() =>
      Promise.resolve(
        create(ListConfiguredExecutionTargetsResponseSchema, {
          executionTargets: [
            applicationTarget("local", "local"),
            applicationTarget("worker", "remote-app", {
              siteManager: false,
              siteNodeDisplayName: "gb10-host-2",
            }),
          ],
        }),
      ),
    );

    renderAppsSection({ client, machines });
    await act(async () => {});
    const table = screen.getByRole("table", { name: "Installed applications" });
    expect(within(table).getByTitle("Machine stale")).toHaveTextContent("Ready");
    expect(within(table).getByText("gb10-host-2")).toBeVisible();

    fireEvent.click(screen.getByRole("row", { name: "OpenWebUI" }));
    await act(async () => {});
    const status = screen.getByRole("region", { name: "Status" });
    expect(within(status).getByRole("status")).toHaveTextContent(
      "Machine stale. Showing the last state gb10-host-2 reported; last report 2 minutes ago.",
    );

    // The machine reports again: the stale notice clears.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2_000);
    });
    expect(screen.queryByTitle("Machine stale")).not.toBeInTheDocument();
    expect(within(status).queryByRole("status")).not.toBeInTheDocument();
  });

  it("follows a running but unready deployment until the controller reports ready", async () => {
    vi.useFakeTimers();
    const application = app("open-webui", "OpenWebUI");
    const unready = deployment(
      application,
      ApplicationLifecycleState.RUNNING,
      ApplicationReadinessState.NOT_READY,
    );
    const ready = deployment(
      application,
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.READY,
    );
    const client = clientFor([], []);
    const responses = [unready, ready];
    const listApplicationDeployments = vi.fn(() =>
      Promise.resolve(
        create(ListApplicationDeploymentsResponseSchema, {
          deployments: [responses.shift() ?? ready],
        }),
      ),
    );
    client.application.listApplicationDeployments = listApplicationDeployments;

    renderAppsSection({ client });
    await act(async () => {});
    expect(screen.getAllByText("Not ready")[0]).toBeVisible();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
    });
    expect(screen.getAllByText("Ready")[0]).toBeVisible();
  });

  it("follows a starting deployment to Ready from the live deployments", async () => {
    vi.useFakeTimers();
    const application = app("open-webui", "OpenWebUI");
    const starting = deployment(
      application,
      ApplicationLifecycleState.STARTING,
      ApplicationReadinessState.NOT_READY,
    );
    const ready = deployment(
      application,
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.READY,
    );
    const client = clientFor([], []);
    const responses = [starting, starting];
    const listApplicationDeployments = vi.fn(() =>
      Promise.resolve(
        create(ListApplicationDeploymentsResponseSchema, {
          deployments: [responses.shift() ?? ready],
        }),
      ),
    );
    client.application.listApplicationDeployments = listApplicationDeployments;

    renderAppsSection({ client });
    await act(async () => {});
    const table = screen.getByRole("table", { name: "Installed applications" });
    expect(within(table).getByText("Starting")).toBeVisible();
    expect(table.querySelector('[data-slot="badge"]')).toHaveAttribute(
      "title",
      "Waiting for endpoint",
    );

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
    });
    expect(within(table).getByText("Starting")).toBeVisible();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
    });
    expect(within(table).queryByText("Starting")).not.toBeInTheDocument();
    expect(within(table).getByText("Ready")).toBeVisible();
  });

  it.each(["Pause", "Resume"] as const)(
    "keeps an accepted %s over live deployments read before it, then follows them",
    async (action) => {
      const application = app(`${action.toLowerCase()}-live-application`, `${action} Live`);
      const id = `${action.toLowerCase()}-live-deployment`;
      const before = lifecycleDeployment(application, id, action === "Pause" ? "ready" : "paused");
      const accepted = lifecycleDeployment(
        application,
        id,
        action === "Pause" ? "pausing" : "starting",
      );
      const settled = lifecycleDeployment(application, id, action === "Pause" ? "paused" : "ready");
      const { client, publish } = renderLive([application], [before]);
      if (action === "Pause") {
        client.application.pauseDeployment = vi.fn(() =>
          Promise.resolve(createPauseResponse(accepted)),
        );
      } else {
        client.application.resumeDeployment = vi.fn(() =>
          Promise.resolve(createResumeResponse(accepted)),
        );
      }
      const acceptedLabel = action === "Pause" ? "Pausing" : "Starting";

      fireEvent.click(await screen.findByRole("row", { name: `${action} Live` }));
      chooseLifecycleAction(action);
      await act(async () => {});
      expect(screen.getAllByText(acceptedLabel)[0]).toBeVisible();

      // Read by the manager before the mutation, delivered after its response.
      await publish([before]);
      expect(screen.getAllByText(acceptedLabel)[0]).toBeVisible();
      expect(screen.queryByRole("link", { name: "Open" })).not.toBeInTheDocument();

      await publish([settled]);
      expect(screen.getAllByText(action === "Pause" ? "Paused" : "Ready")[0]).toBeVisible();
      if (action === "Resume") {
        expect(screen.getByRole("link", { name: "Open" })).toHaveAttribute(
          "href",
          "http://100.64.0.7:49152",
        );
      }
    },
  );

  it.each(["Pause", "Resume"] as const)(
    "lets a live teardown supersede an accepted %s and then closes after deletion",
    async (action) => {
      const application = app(`${action.toLowerCase()}-teardown-application`, `${action} Teardown`);
      const id = `${action.toLowerCase()}-teardown-deployment`;
      const before = lifecycleDeployment(application, id, action === "Pause" ? "ready" : "paused");
      const accepted = lifecycleDeployment(
        application,
        id,
        action === "Pause" ? "pausing" : "starting",
      );
      const { client, publish } = renderLive([application], [before]);
      client.application.pauseDeployment = vi.fn(() =>
        Promise.resolve(createPauseResponse(accepted)),
      );
      client.application.resumeDeployment = vi.fn(() =>
        Promise.resolve(createResumeResponse(accepted)),
      );

      fireEvent.click(await screen.findByRole("row", { name: `${action} Teardown` }));
      chooseLifecycleAction(action);
      await act(async () => {});

      await publish([lifecycleDeployment(application, id, "teardown")]);
      expect(screen.getAllByText("Tearing down")[0]).toBeVisible();

      await publish([]);
      expect(screen.queryByRole("row", { name: `${action} Teardown` })).not.toBeInTheDocument();
    },
  );

  it("drops an accepted Pause once the deployment is gone from the live deployments", async () => {
    const application = app("deleted-application", "Deleted");
    const before = lifecycleDeployment(application, "deleted-deployment", "ready");
    const { client, publish } = renderLive([application], [before]);
    client.application.pauseDeployment = vi.fn(() =>
      Promise.resolve(
        createPauseResponse(lifecycleDeployment(application, "deleted-deployment", "pausing")),
      ),
    );

    fireEvent.click(await screen.findByRole("row", { name: "Deleted" }));
    chooseLifecycleAction("Pause");
    await act(async () => {});
    expect(screen.getAllByText("Pausing")[0]).toBeVisible();

    await publish([]);
    expect(screen.queryByRole("row", { name: "Deleted" })).not.toBeInTheDocument();
  });

  it("lists a deployment created elsewhere without any mutation", async () => {
    const application = app("open-webui", "OpenWebUI");
    const { publish } = renderLive([application], []);
    expect(await screen.findByText("No applications deployed")).toBeVisible();

    await publish([
      deployment(application, ApplicationLifecycleState.READY, ApplicationReadinessState.READY),
    ]);
    expect(screen.getByRole("row", { name: "OpenWebUI" })).toBeVisible();
  });

  it("keeps the last deployments and reports Installed unavailable while live updates fail", async () => {
    const application = app("retry-application", "Retry Application");
    const starting = deployment(
      application,
      ApplicationLifecycleState.STARTING,
      ApplicationReadinessState.NOT_READY,
    );
    const { client, watch } = renderLive([application], [starting]);
    expect(await screen.findByRole("row", { name: "Retry Application" })).toBeVisible();

    client.application.listApplicationDeployments = vi.fn(() =>
      Promise.reject(new Error("temporary service failure")),
    );
    await act(() => watch.tick());

    expect(screen.getByRole("alert", { name: "Installed status" })).toHaveTextContent(
      "Installed deployments are unavailable",
    );
    expect(screen.getByRole("row", { name: "Retry Application" })).toBeVisible();
  });

  it("shows a retained TEARDOWN deployment until it is gone", async () => {
    const application = app("open-webui", "OpenWebUI");
    const tearingDown = lifecycleDeployment(application, "teardown-deployment", "teardown");
    const { publish } = renderLive([], [tearingDown]);

    expect(await screen.findByText("Tearing down")).toBeVisible();
    await publish([]);
    expect(screen.queryByText("Tearing down")).not.toBeInTheDocument();
  });

  it("renders the empty installed state", async () => {
    const client = clientFor([], []);
    client.site.listConfiguredExecutionTargets = vi.fn(() =>
      Promise.resolve(create(ListConfiguredExecutionTargetsResponseSchema)),
    );
    renderAppsSection({ client });

    expect(await screen.findByText("No applications deployed")).toBeVisible();
  });

  it("does not tint its own wrapper, leaving SidebarInset as the sole page canvas", async () => {
    const client = clientFor([], []);
    client.site.listConfiguredExecutionTargets = vi.fn(() =>
      Promise.resolve(create(ListConfiguredExecutionTargetsResponseSchema)),
    );
    const { container } = renderAppsSection({ client });

    await screen.findByText("No applications deployed");
    expect(container.firstElementChild).not.toHaveClass("bg-muted/40");
    expect(container.firstElementChild).not.toHaveClass("min-h-svh");
    expect(container.firstElementChild).not.toHaveClass("models-page");
  });

  it("keeps Installed healthy and removable when catalog loading fails", async () => {
    const user = userEvent.setup();
    const application = app("historic", "Historic App");
    const installed = deployment(
      application,
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.READY,
    );
    const client = clientFor([], [installed]);
    client.catalog.listApplications = vi.fn(() => Promise.reject(new Error("catalog offline")));

    renderAppsSection({ client });

    expect(await screen.findByRole("row", { name: "Historic App" })).toBeVisible();
    expect(screen.queryByRole("alert", { name: "Catalog status" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("row", { name: "Historic App" }));
    expect((await inspectorActionList(user)).menu).toContain("Teardown…");
  });

  it("keeps a deep-linked deployment panel when the first deployment read fails", async () => {
    const client = clientFor([app("historic", "Historic App")], []);
    client.application.listApplicationDeployments = vi.fn(() =>
      Promise.reject(new Error("deployments offline")),
    );

    const { router } = renderAppsSection({
      client,
      initialEntry: "/apps?panel=apps%2Fdeployment%3Adeployment-1",
    });

    expect(await screen.findByRole("alert", { name: "Installed status" })).toHaveTextContent(
      "Installed deployments are unavailable",
    );
    expect(currentLocation(router)).toBe("/apps?panel=apps%2Fdeployment%3Adeployment-1");
  });

  it("selects exactly the deployment row whose inspector is open", async () => {
    const user = userEvent.setup();
    const first = deployment(
      app("first-app", "First App"),
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.READY,
    );
    const second = deployment(
      app("second-app", "Second App"),
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.READY,
    );
    const client = clientFor([], [first, second]);

    renderAppsSection({ client });
    const firstRow = await screen.findByRole("row", { name: "First App" });
    const secondRow = screen.getByRole("row", { name: "Second App" });
    expect(firstRow).not.toHaveAttribute("data-state");
    expect(secondRow).not.toHaveAttribute("data-state");

    await user.click(firstRow);

    expect(firstRow).toHaveAttribute("data-state", "selected");
    expect(secondRow).not.toHaveAttribute("data-state");

    await user.click(screen.getByRole("button", { name: "Close inspector" }));
    await user.click(secondRow);

    expect(secondRow).toHaveAttribute("data-state", "selected");
    expect(firstRow).not.toHaveAttribute("data-state");
  });

  it("marks the deployment row and its inspector with the deployment's own color", async () => {
    const user = userEvent.setup();
    const application = app("installed-app", "Installed App");
    const installed = deployment(
      application,
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.READY,
    );
    renderAppsSection({ client: clientFor([application], [installed]) });

    const row = await screen.findByRole("row", { name: "Installed App" });
    const [appCell, , machineCell] = within(row).getAllByRole("cell");
    const appName = appCell.querySelector(".font-semibold");
    expectResourceMarker(appName, "installed-app-deployment");
    expect(appName).toHaveTextContent(/^Installed App$/);
    expect(appCell).toHaveTextContent("Installed App · 1.0");
    expectResourceMarker(machineCell.querySelector(":scope > span"), "local");
    expect(row).not.toHaveTextContent("installed-app-1");

    await user.click(screen.getByRole("row", { name: "Installed App" }));
    expectResourceMarker(
      inspectorTitle(screen.getByRole("complementary", { name: "Installed App" })),
      "installed-app-deployment",
    );
  });

  it("saves a Name through the application metadata request", async () => {
    const user = userEvent.setup();
    const application = app("editable-app", "Editable App");
    const installed = deployment(
      application,
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.READY,
      "application-deployment-1",
    );
    const accepted = create(ApplicationDeploymentSchema, {
      ...installed,
      applicationDeploymentName: "Renamed App",
    });
    const client = clientFor([], [installed]);
    client.application.updateApplicationDeploymentMetadata = vi.fn(() =>
      Promise.resolve({ displayName: accepted.applicationDeploymentName }),
    );

    renderAppsSection({ client });
    await user.click(await screen.findByRole("row", { name: "Editable App" }));
    await user.click(screen.getByRole("button", { name: "Edit" }));
    const nameInput = screen.getByRole("textbox", { name: "Name" });
    await user.clear(nameInput);
    await user.type(nameInput, "  Renamed App  ");
    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(client.application.updateApplicationDeploymentMetadata).toHaveBeenCalledExactlyOnceWith(
      "application-deployment-1",
      {
        metadata: { displayName: "Renamed App" },
        fields: ["displayName"],
      },
    );
    expect(await screen.findByRole("row", { name: "Renamed App" })).toBeVisible();
  });

  it("does not accept an uncertain Name update from duplicate immutable-ID records", async () => {
    const user = userEvent.setup();
    const application = app("editable-app", "Editable App");
    const installed = deployment(
      application,
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.READY,
      "application-deployment-1",
    );
    const duplicate = create(ApplicationDeploymentSchema, {
      ...installed,
      applicationDeploymentName: "Renamed App",
    });
    const client = clientFor([], [installed]);
    client.application.listApplicationDeployments = vi
      .fn()
      .mockResolvedValueOnce(
        create(ListApplicationDeploymentsResponseSchema, { deployments: [installed] }),
      )
      .mockResolvedValueOnce(
        create(ListApplicationDeploymentsResponseSchema, { deployments: [duplicate, duplicate] }),
      );
    client.application.updateApplicationDeploymentMetadata = vi.fn(() =>
      Promise.reject(new ConnectError("outcome not confirmed", Code.Unavailable)),
    );

    renderAppsSection({ client });
    await user.click(await screen.findByRole("row", { name: "Editable App" }));
    await user.click(screen.getByRole("button", { name: "Edit" }));
    const nameInput = screen.getByRole("textbox", { name: "Name" });
    await user.clear(nameInput);
    await user.type(nameInput, "Renamed App");
    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(
      await screen.findByText(
        "Name could not be saved. Check the deployment status and try again.",
      ),
    ).toBeVisible();
    expect(client.application.updateApplicationDeploymentMetadata).toHaveBeenCalledTimes(1);
    expect(client.application.listApplicationDeployments).toHaveBeenCalledTimes(2);
  });

  it("characterizes application Edit navigation, lifecycle gating, and Name-only section behavior", async () => {
    const user = userEvent.setup();
    const application = app("editable-app", "Editable App");
    const installed = deployment(
      application,
      ApplicationLifecycleState.FAILED,
      ApplicationReadinessState.NOT_READY,
      "application-deployment-1",
    );
    renderAppsSection({ client: clientFor([], [installed]) });

    await user.click(await screen.findByRole("row", { name: "Editable App" }));
    expect(screen.getByRole("button", { name: "Edit" })).toBeEnabled();
    expect(screen.queryByRole("button", { name: "Rename" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Edit" }));

    const editor = screen.getByRole("complementary", { name: "Editable App · Edit" });
    expectResourceMarker(inspectorTitle(editor), "application-deployment-1");
    expect(editor).toHaveTextContent("application-deployment-1");
    expect(within(editor).getByRole("textbox", { name: "Name" })).toHaveAttribute(
      "id",
      "deployment-name",
    );
    expect(within(editor).getAllByText("Name", { exact: true })).toHaveLength(1);
    expect(within(editor).queryByRole("heading", { name: "Name" })).not.toBeInTheDocument();
    expect(within(editor).getByRole("button", { name: "Save" })).toBeEnabled();
    expect(within(editor).queryByRole("button", { name: "Save name" })).not.toBeInTheDocument();
    expect(within(editor).getByRole("button", { name: "Back to Editable App" })).toBeVisible();
  });

  it("labels a deployment's Back with the machine it was opened from", async () => {
    const application = app("editable-app", "Editable App");
    const installed = deployment(
      application,
      ApplicationLifecycleState.RUNNING,
      ApplicationReadinessState.READY,
      "application-deployment-1",
    );
    renderAppsSection({
      client: clientFor([], [installed]),
      initialEntry:
        "/apps?panel=machines%2Fmachine%3Aworker&panel=apps%2Fdeployment%3Aapplication-deployment-1",
    });

    // The URL's placeholder panel gives way to the deployment inspector once it is read.
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Back to machine" })).toBeVisible(),
    );
  });
});

describe("Apps table", () => {
  const GIB = 1024n ** 3n;

  /** A running deployment on a named machine, route and endpoint, measured on two services. */
  function servingDeployment(deviceId = "gpu-0") {
    const application = routeInputApp();
    const serving = deployment(
      application,
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.READY,
    );
    serving.applicationDeploymentName = "Team chat";
    serving.placement = create(ExactPlacementSchema, {
      siteNodeId: "worker",
      executionTargetId: "remote-app",
    });
    serving.selectedInferenceRoute = create(ApplicationSelectedInferenceRouteSchema, {
      inferenceRouteId: "route-1",
      routeName: "private-chat",
    });
    serving.state = create(ApplicationDeploymentStateSchema, {
      endpoint: { url: "http://100.64.0.7:49152" },
      lifecycleState: ApplicationLifecycleState.READY,
      readinessState: ApplicationReadinessState.READY,
    });
    serving.serviceAcceleratorMemory = [
      ["web", 3n * GIB],
      ["worker", 1n * GIB + GIB / 2n],
    ].map(([serviceName, usedMemoryBytes]) =>
      create(ApplicationServiceAcceleratorMemorySchema, {
        acceleratorMemory: {
          result: {
            case: "available",
            value: {
              devices: [
                {
                  deviceId,
                  managedNodeId: "worker",
                  usedMemoryBytes: usedMemoryBytes as bigint,
                },
              ],
            },
          },
        },
        serviceName: serviceName as string,
      }),
    );
    return serving;
  }

  function idleDeployment() {
    const application = app("notes", "Notes");
    return deployment(
      application,
      ApplicationLifecycleState.PAUSED,
      ApplicationReadinessState.UNSPECIFIED,
    );
  }

  function renderTable(initialEntry = "/apps", metrics = resourceWatch()) {
    const serving = servingDeployment();
    const idle = idleDeployment();
    idle.desiredState = ApplicationDeploymentDesiredState.PAUSED;
    return {
      ...renderAppsSection({
        client: clientFor([routeInputApp(), app("notes", "Notes")], [serving, idle]),
        initialEntry,
        machines: [{ displayName: "gb10-host-2", machineId: "worker" }],
        metricsClient: metrics.client,
      }),
      metrics,
    };
  }

  /**
   * A metrics client answering the serving deployment's CPU and memory, and the GPU use of the
   * device it holds memory on, counting the watch streams it holds open.
   */
  function resourceWatch(overrides: Record<string, ReturnType<typeof metricSeries>[]> = {}) {
    const byMetric: Record<string, ReturnType<typeof metricSeries>[]> = {
      "application.host.cpu.utilization": [
        metricSeries({ deployment: "open-webui-deployment" }, [gaugePoint(9, 0.25)]),
      ],
      "application.host.memory.used": [
        metricSeries({ deployment: "open-webui-deployment" }, [gaugePoint(9, 1.5 * 1024 ** 3)]),
      ],
      "node.accelerator.utilization": [
        metricSeries({ device: "gpu-0", node: "worker" }, [gaugePoint(9, 0.6)]),
      ],
      ...overrides,
    };
    const answer = staticWatch((query) =>
      queryResult({ series: byMetric[query?.metric ?? ""] ?? [], stepCount: 10 }),
    );
    let open = 0;
    const watch = vi.fn<MetricsClient["metrics"]["watchMetrics"]>((request, options) => {
      open += 1;
      options?.signal?.addEventListener("abort", () => (open -= 1));
      return answer(request, options);
    });

    return {
      client: metricsClientWith(watch),
      open: () => open,
      queries: (call: number) =>
        (watch.mock.calls[call][0].queries ?? []).map(({ query }) => [
          query?.metric,
          query?.groupBy,
        ]),
      watch,
    };
  }

  const columnTexts = (table: HTMLElement) =>
    within(table)
      .getAllByRole("columnheader")
      .map((header) => header.textContent);

  it("shows each deployment's app, status, machine, endpoint, CPU, RAM, summed VRAM and GPU", async () => {
    renderTable();

    const table = await screen.findByRole("table", { name: "Installed applications" });
    expect(columnTexts(table)).toEqual([
      "App",
      "Status",
      "Machine",
      "Endpoint",
      "CPU",
      "RAM",
      "VRAM",
      "GPU",
    ]);
    const cellTexts = (name: string) =>
      within(within(table).getByRole("row", { name }))
        .getAllByRole("cell")
        .map((cell) => cell.textContent);
    await waitFor(() =>
      expect(cellTexts("Team chat")).toEqual([
        "Team chatOpenWebUI · 1.0",
        "Ready",
        "gb10-host-2",
        "Open",
        "25%",
        "1.5 GiB",
        "4.5 GiB",
        "60%",
      ]),
    );
    // A deployment with no endpoint, reading or measure reads "—".
    expect(cellTexts("Notes")).toEqual([
      "NotesNotes · 1.0",
      "Paused",
      "local",
      "—",
      "—",
      "—",
      "—",
      "—",
    ]);
  });

  it("marks the GPU as shared when another app holds memory on the same device", async () => {
    const other = servingDeployment();
    other.applicationDeploymentId = "other-deployment";
    other.applicationDeploymentName = "Wiki";
    renderAppsSection({
      client: clientFor([routeInputApp()], [servingDeployment(), other]),
      machines: [{ displayName: "gb10-host-2", machineId: "worker" }],
      metricsClient: resourceWatch().client,
    });

    for (const name of ["Team chat", "Wiki"]) {
      const row = await screen.findByRole("row", { name });
      await waitFor(() =>
        expect(within(row).getAllByRole("cell").at(-1)).toHaveTextContent("60%shared"),
      );
    }
  });

  it("keeps the GPU and its shared mark out of an app with no measured device", async () => {
    renderTable();

    const row = await screen.findByRole("row", { name: "Notes" });
    expect(within(row).getAllByRole("cell").at(-1)).toHaveTextContent("—");
    expect(within(row).queryByText("shared")).not.toBeInTheDocument();
  });

  it("watches CPU and memory by deployment and GPU use in two streams", async () => {
    const { metrics } = renderTable();

    await screen.findByRole("table", { name: "Installed applications" });
    await waitFor(() => expect(metrics.open()).toBe(2));
    expect(metrics.queries(0)).toEqual([
      ["application.host.cpu.utilization", ["deployment"]],
      ["application.host.memory.used", ["deployment"]],
    ]);
    expect(metrics.queries(1)).toEqual([["node.accelerator.utilization", ["node", "device"]]]);
    expect(metrics.watch).toHaveBeenCalledTimes(2);
  });

  it("offers Route in the View menu, hidden by default, and keeps it in the URL", async () => {
    const user = userEvent.setup();
    const { router } = renderTable();

    const table = await screen.findByRole("table", { name: "Installed applications" });
    await user.click(screen.getByRole("button", { name: "View" }));
    const route = within(await screen.findByRole("menu")).getByRole("menuitemcheckbox", {
      name: "Route",
    });
    expect(route).not.toBeChecked();

    await user.click(route);
    await user.keyboard("{Escape}");

    expect(columnTexts(table)).toEqual([
      "App",
      "Status",
      "Machine",
      "Endpoint",
      "CPU",
      "RAM",
      "VRAM",
      "GPU",
      "Route",
    ]);
    expect(currentLocation(router)).toBe(
      "/apps?cols=app%2Cstatus%2Cmachine%2Cendpoint%2Ccpu%2Cram%2Cvram%2Cgpu%2Croute",
    );
    expect(
      within(within(table).getByRole("row", { name: "Team chat" }))
        .getAllByRole("cell")
        .at(-1),
    ).toHaveTextContent("private-chat");
    cleanup();

    renderTable(currentLocation(router));
    const restored = await screen.findByRole("table", { name: "Installed applications" });
    expect(columnTexts(restored).at(-1)).toBe("Route");
  });

  it.each([
    ["cpu", ["Team chat", "Wiki", "Notes"], ["Wiki", "Team chat", "Notes"]],
    ["ram", ["Wiki", "Team chat", "Notes"], ["Team chat", "Wiki", "Notes"]],
    ["gpu", ["Wiki", "Team chat", "Notes"], ["Team chat", "Wiki", "Notes"]],
  ])("sorts by %s value, apps without a value last", async (key, ascending, descending) => {
    const wiki = servingDeployment("gpu-1");
    wiki.applicationDeploymentId = "wiki-deployment";
    wiki.applicationDeploymentName = "Wiki";
    const notes = idleDeployment();
    const metrics = resourceWatch({
      "application.host.cpu.utilization": [
        metricSeries({ deployment: "open-webui-deployment" }, [gaugePoint(9, 0.25)]),
        metricSeries({ deployment: "wiki-deployment" }, [gaugePoint(9, 0.5)]),
      ],
      "application.host.memory.used": [
        metricSeries({ deployment: "open-webui-deployment" }, [gaugePoint(9, 3 * 1024 ** 3)]),
        metricSeries({ deployment: "wiki-deployment" }, [gaugePoint(9, 1024 ** 3)]),
      ],
      "node.accelerator.utilization": [
        metricSeries({ device: "gpu-0", node: "worker" }, [gaugePoint(9, 0.6)]),
        metricSeries({ device: "gpu-1", node: "worker" }, [gaugePoint(9, 0.2)]),
      ],
    });
    const names = () =>
      screen
        .getAllByRole("row")
        .slice(1)
        .map((row) => row.getAttribute("aria-label"));
    const render = (sort: string) =>
      renderAppsSection({
        client: clientFor(
          [routeInputApp(), app("notes", "Notes")],
          [servingDeployment(), wiki, notes],
        ),
        initialEntry: `/apps?sort=${sort}`,
        machines: [{ displayName: "gb10-host-2", machineId: "worker" }],
        metricsClient: metrics.client,
      });

    render(key);
    await waitFor(() => expect(names()).toEqual(ascending));
    cleanup();

    render(`-${key}`);
    await waitFor(() => expect(names()).toEqual(descending));
  });

  it("filters by status and machine from the view", async () => {
    renderTable("/apps?status=Paused");
    expect(await screen.findByRole("row", { name: "Notes" })).toBeVisible();
    expect(screen.queryByRole("row", { name: "Team chat" })).not.toBeInTheDocument();
    cleanup();

    renderTable("/apps?machine=gb10-host-2");
    expect(await screen.findByRole("row", { name: "Team chat" })).toBeVisible();
    expect(screen.queryByRole("row", { name: "Notes" })).not.toBeInTheDocument();
  });
});

/**
 * Renders the Apps page over live deployments that `publish` replaces and delivers as the next
 * manager tick.
 */
function renderLive(applications: ReturnType<typeof app>[], initial: ApplicationDeployment[]) {
  let live = initial;
  const client = clientFor(applications, []);
  client.application.listApplicationDeployments = vi.fn(() =>
    Promise.resolve(create(ListApplicationDeploymentsResponseSchema, { deployments: live })),
  );
  const { watch } = renderAppsSection({ client });
  return {
    client,
    publish: async (next: ApplicationDeployment[]) => {
      live = next;
      await act(() => watch.tick());
    },
    watch,
  };
}

/** One deployment of `application` in a named lifecycle step. */
function lifecycleDeployment(
  application: ReturnType<typeof app>,
  applicationDeploymentId: string,
  step: "paused" | "pausing" | "ready" | "starting" | "teardown",
): ApplicationDeployment {
  const desiredState = {
    paused: ApplicationDeploymentDesiredState.PAUSED,
    pausing: ApplicationDeploymentDesiredState.PAUSED,
    ready: ApplicationDeploymentDesiredState.PRESENT,
    starting: ApplicationDeploymentDesiredState.PRESENT,
    teardown: ApplicationDeploymentDesiredState.TEARDOWN,
  }[step];
  const lifecycleState = {
    paused: ApplicationLifecycleState.PAUSED,
    pausing: ApplicationLifecycleState.PAUSING,
    ready: ApplicationLifecycleState.READY,
    starting: ApplicationLifecycleState.STARTING,
    teardown: ApplicationLifecycleState.STOPPING,
  }[step];
  return create(ApplicationDeploymentSchema, {
    ...deployment(
      application,
      lifecycleState,
      ApplicationReadinessState.NOT_READY,
      applicationDeploymentId,
    ),
    desiredState,
    state: create(ApplicationDeploymentStateSchema, {
      endpoint: step === "ready" ? { url: "http://100.64.0.7:49152" } : undefined,
      lifecycleState,
      readinessState:
        step === "ready" ? ApplicationReadinessState.READY : ApplicationReadinessState.NOT_READY,
    }),
  });
}

function createPauseResponse(deploymentValue: ReturnType<typeof deployment>) {
  return create(PauseDeploymentResponseSchema, { deployment: deploymentValue });
}

function createResumeResponse(deploymentValue: ReturnType<typeof deployment>) {
  return create(ResumeDeploymentResponseSchema, { deployment: deploymentValue });
}

function createRemoveResponse(deploymentValue?: ReturnType<typeof deployment>) {
  return create(RemoveApplicationDeploymentResponseSchema, { deployment: deploymentValue });
}

/** Pause sits in the inspector's more-actions menu; Resume is the inspector's primary button. */
function chooseLifecycleAction(action: "Pause" | "Resume") {
  if (action === "Pause") pickMoreAction("Pause");
  else fireEvent.click(screen.getByRole("button", { name: "Resume" }));
}
