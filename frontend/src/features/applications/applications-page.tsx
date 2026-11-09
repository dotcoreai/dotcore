import { ConnectError } from "@connectrpc/connect";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useLocation, useNavigate } from "react-router-dom";

import { type ApplicationClient, applicationClient } from "@/api/application-client";
import { type MetricsClient, metricsClient as defaultMetricsClient } from "@/api/metrics-client";
import { DataTable } from "@/components/data-table/data-table";
import { buildDataTable } from "@/components/data-table/data-table-model";
import { DataTableToolbar } from "@/components/data-table/data-table-toolbar";
import { useDataTableView } from "@/components/data-table/use-data-table-view";
import { DeployActionIcon } from "@/components/ui/action-icons";
import { AddAction } from "@/components/ui/add-action";
import { EmptyState } from "@/components/ui/empty-state";
import { InspectorPlaceholder } from "@/components/ui/inspector-placeholder";
import { Notice } from "@/components/ui/notice";
import {
  PageActions,
  PageHeader,
  PageHeaderHeading,
  PageSectionHeading,
} from "@/components/ui/page-header";
import { AppsSectionIcon } from "@/components/ui/section-icons";
import { DeploymentApplicationInspector } from "@/features/applications/application-inspector";
import { applicationInspectorPanelId } from "@/features/applications/application-inspector-panel";
import { describeMutationError } from "@/features/applications/application-mutation-errors";
import { DeploymentLogViewer } from "@/features/deployment-logs/deployment-log-viewer";
import { DeploymentEditor } from "@/features/deployments/deployment-editor";
import { placementMachine } from "@/features/deployments/placement";
import {
  type ApplicationDeployment,
  ApplicationDeploymentDesiredState,
} from "@/gen/dotcore/application/v1/deployment_pb";
import { ApplicationLifecycleState } from "@/gen/dotcore/application/v1/state_pb";
import type { Application } from "@/gen/dotcore/catalog/v1/application_pb";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";
import { applicationDeploymentPresentationName } from "@/lib/application-deployment-name";
import type { DeploymentMetadata, DeploymentMetadataUpdate } from "@/lib/deployment-metadata";
import { rejectedMutationCodes } from "@/lib/mutation-errors";
import { usePanelNavigation } from "@/panels/panel-navigation";
import { usePanelPendingNavigation } from "@/panels/panel-navigation-pending";
import {
  activePanel,
  appendPanel,
  appendPanelHref,
  canonicalizePanelLocation,
  type PanelEntry,
  type PanelLocation,
  panelPagePath,
  selectPanel,
} from "@/panels/panel-stack";
import { panelTitle } from "@/panels/panel-titles";
import { usePublishPanelTitles } from "@/panels/panel-titles-context";

import { projectApplicationDeploymentRows } from "./application-deployment-row";
import {
  applicationDeploymentTableDefinition,
  applicationDeploymentTableUi,
  applicationDeploymentViewCodec,
} from "./application-deployment-table";
import { useSharedApplicationsPolling } from "./applications-polling-context";
import { useApplicationResources } from "./use-application-resources";

export interface ApplicationsPageProps {
  client?: ApplicationClient;
  metricsClient?: MetricsClient;
}

export interface ApplicationInspectorSelection {
  applicationDeploymentId: string;
}

function applicationSelectionFromEntry(
  entry: PanelEntry | undefined,
): ApplicationInspectorSelection | undefined {
  if (!entry || entry.page !== "apps" || !entry.resourceId) return undefined;
  return { applicationDeploymentId: entry.resourceId };
}

function applicationSelectionEntry(selection: ApplicationInspectorSelection): PanelEntry {
  return { page: "apps", resourceId: selection.applicationDeploymentId, screen: "deployment" };
}

interface MutationError {
  message: string;
  selection: ApplicationInspectorSelection;
}

function isSameSelection(
  left: ApplicationInspectorSelection | undefined,
  right: ApplicationInspectorSelection | undefined,
): boolean {
  return Boolean(left && right && left.applicationDeploymentId === right.applicationDeploymentId);
}

function deploymentPendingKey(
  applicationDeploymentId: string,
  action: "pause" | "resume" | "remove" | "update-metadata",
): string {
  return `deployment:${applicationDeploymentId}:${action}`;
}

const noDeployments: readonly ApplicationDeployment[] = [];

const deploymentActions = ["pause", "resume", "remove", "update-metadata"] as const;

function isDeploymentPending(
  pendingActions: ReadonlySet<string>,
  applicationDeploymentId: string,
): boolean {
  return deploymentActions.some((action) =>
    pendingActions.has(deploymentPendingKey(applicationDeploymentId, action)),
  );
}

export function ApplicationsPage({
  client = applicationClient,
  metricsClient = defaultMetricsClient,
}: ApplicationsPageProps) {
  const location = useLocation();
  const navigate = useNavigate();
  const panelNavigation = usePanelNavigation();
  const [selection, setSelection] = useState<ApplicationInspectorSelection>();
  const view = useDataTableView(applicationDeploymentViewCodec);
  const canonical = useMemo(
    () => canonicalizePanelLocation({ pathname: location.pathname, search: location.search }),
    [location.pathname, location.search],
  );
  const urlEntry = activePanel(canonical.search);
  const urlSelection = applicationSelectionFromEntry(urlEntry);
  const viewingLogs = urlEntry?.page === "apps" && urlEntry.screen === "logs";
  const editingDeployment = urlEntry?.page === "apps" && urlEntry.screen === "edit";
  const [mutationError, setMutationError] = useState<MutationError>();
  const [pendingActions, setPendingActions] = useState<ReadonlySet<string>>(() => new Set());
  const pendingActionsRef = useRef<Set<string>>(new Set());
  const openerRef = useRef<HTMLElement | null>(null);
  const logOpenerRef = useRef<HTMLButtonElement>(null);
  const headingRef = useRef<HTMLHeadingElement>(null);
  const selectionRef = useRef<ApplicationInspectorSelection | undefined>(undefined);
  const pendingSelectionRef = useRef<ApplicationInspectorSelection | undefined>(undefined);
  const shouldRestoreHeadingFocus = useRef(false);
  const closeSelection = useCallback(() => {
    pendingSelectionRef.current = undefined;
    selectionRef.current = undefined;
    setMutationError(undefined);
    setSelection(undefined);
    panelNavigation.close();
  }, [panelNavigation]);
  const requestCloseSelection = useCallback(() => {
    panelNavigation.close();
  }, [panelNavigation]);
  useEffect(() => {
    if (canonical.recovered || (urlEntry && !urlSelection)) {
      // The shown view wins over the one the location was read with; another page takes none.
      void navigate(
        canonical.pathname === location.pathname
          ? {
              ...canonical,
              search: applicationDeploymentViewCodec.apply(canonical.search, view.state),
            }
          : canonical,
        { replace: true },
      );
      return;
    }

    if (pendingSelectionRef.current) {
      if (isSameSelection(pendingSelectionRef.current, urlSelection)) {
        pendingSelectionRef.current = undefined;
      } else {
        return;
      }
    }

    if (!isSameSelection(selectionRef.current, urlSelection)) {
      selectionRef.current = urlSelection;
      setMutationError(undefined);
      setSelection(urlSelection);
    }
  }, [canonical, location.pathname, navigate, urlEntry, urlSelection, view.state]);

  const { applyDeploymentMetadataOverlay, applyDeploymentOverlay, snapshot, statuses } =
    useSharedApplicationsPolling();
  const deployments = snapshot?.deployments ?? noDeployments;
  const resources = useApplicationResources(metricsClient, deployments.length > 0);
  const deploymentRows = useMemo(
    () =>
      snapshot
        ? projectApplicationDeploymentRows(snapshot.deployments, {
            machines: snapshot.machines,
            resources,
          })
        : [],
    [resources, snapshot],
  );
  const deploymentTable = useMemo(
    () => buildDataTable(applicationDeploymentTableDefinition, deploymentRows, view.state),
    [deploymentRows, view.state],
  );
  const loadingFeedbackVisible = useDelayedLoading(!snapshot);
  const initialLoadFailed = Boolean(
    statuses.catalog.error || statuses.deployments.error || statuses.executionTargets.error,
  );
  const selectionError =
    mutationError && isSameSelection(mutationError.selection, selection)
      ? mutationError.message
      : undefined;

  const selectedDeployment = selection
    ? snapshot?.deployments.find(
        (deployment) => deployment.applicationDeploymentId === selection.applicationDeploymentId,
      )
    : undefined;
  usePublishPanelTitles([
    ...(snapshot?.deployments ?? []).map((deployment) =>
      panelTitle(
        "apps",
        "deployment",
        deployment.applicationDeploymentId,
        applicationDeploymentPresentationName(deployment),
      ),
    ),
    ...(snapshot?.catalogApplications ?? []).map((application) =>
      panelTitle("app-catalog", "catalog", application.applicationId, application.displayName),
    ),
    ...(snapshot?.machines ?? []).map((machine) =>
      panelTitle(
        "machines",
        "machine",
        machine.machineId,
        machine.displayName || machine.machineId,
      ),
    ),
  ]);
  const selectedNavigationPending = selection
    ? isDeploymentPending(pendingActions, selection.applicationDeploymentId)
    : false;
  usePanelPendingNavigation(
    selectedNavigationPending,
    "An inspector operation is pending. Inspector navigation is unavailable.",
  );
  const selectedPlacementMachine = placementMachine(
    snapshot?.machines,
    selectedDeployment?.placement?.siteNodeId,
  );
  const executionTargets = statuses.executionTargets.hasData
    ? snapshot?.executionTargets
    : undefined;

  useEffect(() => {
    // Only loaded deployments can show the deployment is gone; a failed first read keeps the panel.
    if (!selection || !snapshot || !statuses.deployments.hasData) {
      return;
    }

    const deployment = snapshot.deployments.find(
      (candidate) => candidate.applicationDeploymentId === selection.applicationDeploymentId,
    );
    if (!deployment) {
      queueMicrotask(() => {
        if (isSameSelection(selectionRef.current, selection)) {
          openerRef.current = headingRef.current;
          shouldRestoreHeadingFocus.current = true;
          closeSelection();
        }
      });
    }
  }, [closeSelection, selection, snapshot, statuses.deployments.hasData]);

  useEffect(() => {
    if (!selection && shouldRestoreHeadingFocus.current) {
      headingRef.current?.focus();
      shouldRestoreHeadingFocus.current = false;
    }
  }, [selection]);

  const updatePending = (key: string, pending: boolean) => {
    if (pending) {
      pendingActionsRef.current.add(key);
    } else {
      pendingActionsRef.current.delete(key);
    }
    setPendingActions(new Set(pendingActionsRef.current));
  };

  const remove = async (deployment: ApplicationDeployment) => {
    const initiatingSelection: ApplicationInspectorSelection = {
      applicationDeploymentId: deployment.applicationDeploymentId,
    };
    const pendingKey = deploymentPendingKey(deployment.applicationDeploymentId, "remove");
    if (pendingActionsRef.current.has(pendingKey)) {
      return;
    }

    updatePending(pendingKey, true);
    try {
      const response = await client.application.removeApplicationDeployment({
        applicationDeploymentId: deployment.applicationDeploymentId,
      });
      const acceptedDeployment = response.deployment;

      if (
        !acceptedDeployment ||
        acceptedDeployment.applicationDeploymentId !== deployment.applicationDeploymentId
      ) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            message:
              "Teardown response protocol error: missing or mismatched accepted deployment ID.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      if (acceptedDeployment.desiredState !== ApplicationDeploymentDesiredState.TEARDOWN) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            message: "Teardown response protocol error: accepted deployment is not TEARDOWN.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      applyDeploymentOverlay(acceptedDeployment);
    } catch (mutationError) {
      if (isSameSelection(selectionRef.current, initiatingSelection)) {
        setMutationError({
          message: describeMutationError("Teardown", mutationError),
          selection: initiatingSelection,
        });
      }
    } finally {
      updatePending(pendingKey, false);
    }
  };

  const pause = async (deployment: ApplicationDeployment) => {
    const initiatingSelection: ApplicationInspectorSelection = {
      applicationDeploymentId: deployment.applicationDeploymentId,
    };
    const pendingKey = deploymentPendingKey(deployment.applicationDeploymentId, "pause");
    if (pendingActionsRef.current.has(pendingKey)) {
      return;
    }

    updatePending(pendingKey, true);
    try {
      const response = await client.application.pauseDeployment({
        applicationDeploymentId: deployment.applicationDeploymentId,
      });
      const acceptedDeployment = response.deployment;

      if (
        !acceptedDeployment ||
        acceptedDeployment.applicationDeploymentId !== deployment.applicationDeploymentId
      ) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            message: "Pause response protocol error: missing or mismatched accepted deployment ID.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      if (acceptedDeployment.desiredState !== ApplicationDeploymentDesiredState.PAUSED) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            message: "Pause response protocol error: accepted deployment is not PAUSED.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      if (
        !acceptedDeployment.state ||
        !new Set([
          ApplicationLifecycleState.PAUSING,
          ApplicationLifecycleState.PAUSED,
          ApplicationLifecycleState.UNAVAILABLE,
        ]).has(acceptedDeployment.state.lifecycleState)
      ) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            message:
              "Pause response protocol error: accepted deployment has an invalid lifecycle state.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      applyDeploymentOverlay(acceptedDeployment);
    } catch (mutationError) {
      if (isSameSelection(selectionRef.current, initiatingSelection)) {
        setMutationError({
          message: describeMutationError("Pause", mutationError),
          selection: initiatingSelection,
        });
      }
    } finally {
      updatePending(pendingKey, false);
    }
  };

  const resume = async (deployment: ApplicationDeployment) => {
    const initiatingSelection: ApplicationInspectorSelection = {
      applicationDeploymentId: deployment.applicationDeploymentId,
    };
    const pendingKey = deploymentPendingKey(deployment.applicationDeploymentId, "resume");
    if (pendingActionsRef.current.has(pendingKey)) {
      return;
    }

    updatePending(pendingKey, true);
    try {
      const response = await client.application.resumeDeployment({
        applicationDeploymentId: deployment.applicationDeploymentId,
      });
      const acceptedDeployment = response.deployment;

      if (
        !acceptedDeployment ||
        acceptedDeployment.applicationDeploymentId !== deployment.applicationDeploymentId
      ) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            message:
              "Resume response protocol error: missing or mismatched accepted deployment ID.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      if (acceptedDeployment.desiredState !== ApplicationDeploymentDesiredState.PRESENT) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            message: "Resume response protocol error: accepted deployment is not PRESENT.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      if (
        !acceptedDeployment.state ||
        !new Set([
          ApplicationLifecycleState.QUEUED,
          ApplicationLifecycleState.PREPARING,
          ApplicationLifecycleState.STARTING,
          ApplicationLifecycleState.RUNNING,
          ApplicationLifecycleState.READY,
          ApplicationLifecycleState.FAILED,
          ApplicationLifecycleState.UNAVAILABLE,
        ]).has(acceptedDeployment.state.lifecycleState)
      ) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            message:
              "Resume response protocol error: accepted deployment has an invalid lifecycle state.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      applyDeploymentOverlay(acceptedDeployment);
    } catch (mutationError) {
      if (isSameSelection(selectionRef.current, initiatingSelection)) {
        setMutationError({
          message: describeMutationError("Resume", mutationError),
          selection: initiatingSelection,
        });
      }
    } finally {
      updatePending(pendingKey, false);
    }
  };

  const updateMetadata = async (
    deployment: ApplicationDeployment,
    update: DeploymentMetadataUpdate,
  ): Promise<DeploymentMetadata> => {
    const initiatingSelection: ApplicationInspectorSelection = {
      applicationDeploymentId: deployment.applicationDeploymentId,
    };
    const requested = update.metadata;
    const pendingKey = deploymentPendingKey(deployment.applicationDeploymentId, "update-metadata");
    if (pendingActionsRef.current.has(pendingKey)) {
      throw new Error("Metadata save is already pending.");
    }
    updatePending(pendingKey, true);
    try {
      const acceptedMetadata = await client.application.updateApplicationDeploymentMetadata(
        deployment.applicationDeploymentId,
        update,
      );
      applyDeploymentMetadataOverlay(deployment.applicationDeploymentId, acceptedMetadata);
      return acceptedMetadata;
    } catch (error) {
      const connectError = ConnectError.from(error);
      if (!rejectedMutationCodes.has(connectError.code)) {
        try {
          const refreshed = await client.application.listApplicationDeployments({});
          const matches = refreshed.deployments.filter(
            (candidate) => candidate.applicationDeploymentId === deployment.applicationDeploymentId,
          );
          const observed = matches[0];
          const observedMetadata: DeploymentMetadata | undefined = observed
            ? { displayName: observed.applicationDeploymentName }
            : undefined;
          if (
            matches.length === 1 &&
            observedMetadata &&
            update.fields.every((field) => observedMetadata[field] === requested[field])
          ) {
            applyDeploymentMetadataOverlay(deployment.applicationDeploymentId, observedMetadata);
            return observedMetadata;
          }
        } catch {
          // Preserve the uncertain outcome when the authoritative read also fails.
        }
      }
      if (isSameSelection(selectionRef.current, initiatingSelection)) {
        setMutationError({
          message: describeMutationError("Update metadata", error),
          selection: initiatingSelection,
        });
      }
    } finally {
      updatePending(pendingKey, false);
    }
    throw new Error("Name could not be saved. Check the deployment status and try again.");
  };

  const select = (nextSelection: ApplicationInspectorSelection, opener: HTMLElement) => {
    openerRef.current = opener;
    pendingSelectionRef.current = nextSelection;
    selectionRef.current = nextSelection;
    setMutationError(undefined);
    setSelection(nextSelection);
    panelNavigation.navigatePanel({
      pathname: "/apps",
      search: selectPanel(location.search, applicationSelectionEntry(nextSelection)),
    });
  };

  const onInspectorOpenChange = (open: boolean) => {
    if (!open) {
      requestCloseSelection();
    }
  };

  return (
    <div className="min-w-0">
      <div className="flex w-full flex-col gap-6">
        <PageHeader>
          <PageHeaderHeading icon={AppsSectionIcon} ref={headingRef} tabIndex={-1}>
            Apps
          </PageHeaderHeading>
          <PageActions>
            <AddAction icon={DeployActionIcon} label="Deploy" to={panelPagePath("app-catalog")} />
          </PageActions>
        </PageHeader>
        {loadingFeedbackVisible && !initialLoadFailed ? (
          <p className="models-page-feedback text-sm text-muted-foreground" role="status">
            Loading apps…
          </p>
        ) : null}

        {snapshot && (!loadingFeedbackVisible || initialLoadFailed) ? (
          <>
            <section
              aria-labelledby="installed-applications-heading"
              className="flex flex-col gap-4"
            >
              <div>
                <PageSectionHeading id="installed-applications-heading">
                  Installed
                </PageSectionHeading>
              </div>
              {statuses.deployments.error ? (
                <Notice aria-label="Installed status" role="alert" tone="destructive">
                  {statuses.deployments.error}
                </Notice>
              ) : null}
              {snapshot.deployments.length ? (
                <div className="flex min-w-0 flex-col gap-2">
                  <DataTableToolbar
                    facets={deploymentTable.facets}
                    onViewChange={view.set}
                    ui={applicationDeploymentTableUi}
                    view={view.state}
                  />
                  <DataTable
                    onOpenRow={({ row }, opener) =>
                      select({ applicationDeploymentId: row.deploymentId }, opener)
                    }
                    onViewChange={view.set}
                    selection={selection ? { rowId: selection.applicationDeploymentId } : undefined}
                    table={deploymentTable}
                    ui={applicationDeploymentTableUi}
                    view={view.state}
                  />
                </div>
              ) : (
                <EmptyState
                  action={
                    <AddAction
                      icon={DeployActionIcon}
                      label="Deploy"
                      to={panelPagePath("app-catalog")}
                    />
                  }
                  description="Deploy an application from the catalog to run it on your site."
                  icon={AppsSectionIcon}
                  title="No applications deployed"
                />
              )}
            </section>
          </>
        ) : null}

        {urlEntry && urlSelection && !selectedDeployment ? (
          <InspectorPlaceholder
            entry={urlEntry}
            error={statuses.deployments.hasData ? undefined : statuses.deployments.error}
            fallbackTitle="Application deployment"
            id={applicationInspectorPanelId}
            onBack={() => panelNavigation.back()}
            onOpenChange={onInspectorOpenChange}
            openerRef={openerRef}
          />
        ) : null}

        {selection && selectedDeployment && !editingDeployment ? (
          <DeploymentApplicationInspector
            executionTargets={executionTargets}
            onBack={() => panelNavigation.back()}
            applicationHref={catalogApplicationHref(
              selectedDeployment,
              snapshot?.catalogApplications,
              location,
            )}
            deployment={selectedDeployment}
            error={selectionError}
            floatingLayer={
              viewingLogs && client.logs ? (
                <DeploymentLogViewer
                  colorKey={selectedDeployment.applicationDeploymentId}
                  deploymentId={selectedDeployment.applicationDeploymentId}
                  name={applicationDeploymentPresentationName(selectedDeployment)}
                  onOpenChange={(open) => {
                    if (!open) panelNavigation.back();
                  }}
                  open
                  openerRef={logOpenerRef}
                  operations={client.logs}
                />
              ) : null
            }
            inferenceRouteHref={
              selectedDeployment.selectedInferenceRoute
                ? appendPanelHref(location, {
                    page: "routes",
                    resourceId: selectedDeployment.selectedInferenceRoute.inferenceRouteId,
                    screen: "route",
                  })
                : undefined
            }
            logOpenerRef={logOpenerRef}
            logs={client.logs}
            onOpenLogs={() =>
              panelNavigation.navigatePanel({
                pathname: "/apps",
                search: selectPanel(location.search, {
                  page: "apps",
                  resourceId: selectedDeployment.applicationDeploymentId,
                  screen: "logs",
                }),
              })
            }
            onPause={() => void pause(selectedDeployment)}
            onEdit={() =>
              panelNavigation.navigatePanel({
                pathname: "/apps",
                search: appendPanel(location.search, {
                  page: "apps",
                  resourceId: selectedDeployment.applicationDeploymentId,
                  screen: "edit",
                }),
              })
            }
            onOpenChange={onInspectorOpenChange}
            onRemove={() => void remove(selectedDeployment)}
            onResume={() => void resume(selectedDeployment)}
            open={Boolean(urlSelection)}
            openerRef={openerRef}
            pending={isDeploymentPending(
              pendingActions,
              selectedDeployment.applicationDeploymentId,
            )}
            placementMachine={selectedPlacementMachine}
            placementMachineHref={
              selectedPlacementMachine
                ? appendPanelHref(location, {
                    page: "machines",
                    resourceId: selectedPlacementMachine.machineId,
                    screen: "machine",
                  })
                : undefined
            }
            pendingAction={
              pendingActions.has(
                deploymentPendingKey(selectedDeployment.applicationDeploymentId, "pause"),
              )
                ? "pause"
                : pendingActions.has(
                      deploymentPendingKey(selectedDeployment.applicationDeploymentId, "resume"),
                    )
                  ? "resume"
                  : pendingActions.has(
                        deploymentPendingKey(selectedDeployment.applicationDeploymentId, "remove"),
                      )
                    ? "remove"
                    : undefined
            }
            suspendDismissal={viewingLogs}
          />
        ) : null}
        {selection && selectedDeployment && editingDeployment ? (
          <DeploymentEditor
            colorKey={selectedDeployment.applicationDeploymentId}
            deploymentId={selectedDeployment.applicationDeploymentId}
            metadata={{ displayName: applicationDeploymentPresentationName(selectedDeployment) }}
            name={applicationDeploymentPresentationName(selectedDeployment)}
            onClose={requestCloseSelection}
            onSaveMetadata={(update) => updateMetadata(selectedDeployment, update)}
            open
            openerRef={openerRef}
            terminal={
              selectedDeployment.desiredState === ApplicationDeploymentDesiredState.TEARDOWN
            }
          />
        ) : null}
      </div>
    </div>
  );
}

/**
 * Destination of a deployment's catalog application mention, pushed above the deployment panel on
 * the Catalog page. There is none once the Catalog no longer lists the application.
 */
function catalogApplicationHref(
  deployment: ApplicationDeployment,
  catalogApplications: readonly Application[] | undefined,
  location: PanelLocation,
): string | undefined {
  const applicationId = deployment.snapshot?.applicationId;
  const listed = catalogApplications?.some(
    (application) => application.applicationId === applicationId,
  );
  if (!applicationId || !listed) return undefined;

  return appendPanelHref(location, {
    page: "app-catalog",
    resourceId: applicationId,
    screen: "catalog",
  });
}
