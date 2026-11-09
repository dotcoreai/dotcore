import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useLocation, useNavigate } from "react-router-dom";

import { type ApplicationClient, applicationClient } from "@/api/application-client";
import { usePanelNavigation } from "@/app/console-layout";
import { CatalogSectionIcon } from "@/app/console-section-icons";
import { useWatchedRoutes } from "@/app/resource-watch";
import { DataTable } from "@/components/data-table/data-table";
import { DataTableToolbar } from "@/components/data-table/data-table-toolbar";
import { useDataTableView } from "@/components/data-table/use-data-table-view";
import { Notice } from "@/components/ui/notice";
import { PageHeader, PageHeaderHeading } from "@/components/ui/page-header";
import type { Application } from "@/gen/dotcore/catalog/v1/application_pb";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";
import { applicationDeploymentPresentationName } from "@/lib/application-deployment-name";
import { buildDataTable } from "@/lib/data-table-model";
import { usePanelPendingNavigation } from "@/panels/panel-navigation-pending";
import {
  activePanel,
  appendPanelLocation,
  canonicalizePanelLocation,
  deepensPanelStack,
  isSamePanelEntry,
  type PanelEntry,
  panelPagePath,
  selectPanel,
  serializePanelStack,
} from "@/panels/panel-stack";
import { panelTitle } from "@/panels/panel-titles";
import { usePublishPanelTitles } from "@/panels/panel-titles-context";

import {
  ApplicationDeployDialog,
  CatalogApplicationInspector,
  type DeploymentCreationOptions,
} from "./app-catalog-inspector";
import { describeMutationError } from "./application-mutation-errors";
import { useSharedApplicationsPolling } from "./applications-polling-context";
import {
  catalogApplicationDeployments,
  projectCatalogApplicationRows,
} from "./catalog-application-row";
import {
  catalogApplicationTableDefinition,
  catalogApplicationTableUi,
  catalogApplicationViewCodec,
} from "./catalog-application-table";

export interface AppCatalogPageProps {
  client?: ApplicationClient;
}

/**
 * The Application Catalog page: the catalog applications as a table, their inspector, and the
 * deploy dialog.
 * It owns every application deployment creation; an accepted application deployment opens its
 * panel on the Apps page.
 */
export function AppCatalogPage({ client = applicationClient }: AppCatalogPageProps) {
  const location = useLocation();
  const navigate = useNavigate();
  const panelNavigation = usePanelNavigation();
  const [applicationId, setApplicationId] = useState<string>();
  const canonical = useMemo(
    () => canonicalizePanelLocation({ pathname: location.pathname, search: location.search }),
    [location.pathname, location.search],
  );
  const urlEntry = activePanel(canonical.search);
  const urlApplicationId = applicationIdFromEntry(urlEntry);
  const [deployDialogOpen, setDeployDialogOpen] = useState(false);
  const [mutationError, setMutationError] = useState<MutationError>();
  const [pendingActions, setPendingActions] = useState<ReadonlySet<string>>(() => new Set());
  const pendingActionsRef = useRef<Set<string>>(new Set());
  const openerRef = useRef<HTMLElement | null>(null);
  const deployDialogOpenerRef = useRef<HTMLButtonElement>(null);
  const headingRef = useRef<HTMLHeadingElement>(null);
  const applicationIdRef = useRef<string | undefined>(undefined);
  const pendingApplicationIdRef = useRef<string | undefined>(undefined);
  const shouldRestoreHeadingFocus = useRef(false);
  const mountedRef = useRef(false);
  const { applyDeploymentOverlay, snapshot, statuses } = useSharedApplicationsPolling();
  const view = useDataTableView(catalogApplicationViewCodec);
  const applicationRows = useMemo(
    () =>
      snapshot
        ? projectCatalogApplicationRows({
            applications: snapshot.catalogApplications,
            deployments: snapshot.deployments,
          })
        : [],
    [snapshot],
  );
  const applicationTable = useMemo(
    () => buildDataTable(catalogApplicationTableDefinition, applicationRows, view.state),
    [applicationRows, view.state],
  );
  const listConfiguredExecutionTargets = useCallback(
    () => client.site.listConfiguredExecutionTargets({}),
    [client.site],
  );
  const loadingFeedbackVisible = useDelayedLoading(!snapshot);
  const initialLoadFailed = Boolean(
    statuses.catalog.error || statuses.deployments.error || statuses.executionTargets.error,
  );
  const watchedRoutes = useWatchedRoutes();
  const routes = watchedRoutes?.value?.routes ?? noRoutes;
  const routesCurrent = watchedRoutes?.value !== undefined && !watchedRoutes.failed;
  const routeError = watchedRoutes?.failed ? routesUnavailableMessage : undefined;
  const closeSelection = useCallback(() => {
    setDeployDialogOpen(false);
    pendingApplicationIdRef.current = undefined;
    applicationIdRef.current = undefined;
    setMutationError(undefined);
    setApplicationId(undefined);
    panelNavigation.close();
  }, [panelNavigation]);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  useEffect(() => {
    if (canonical.recovered || (urlEntry && !urlApplicationId)) {
      // The shown view wins over the one the location was read with; another page takes none.
      void navigate(
        canonical.pathname === location.pathname
          ? {
              ...canonical,
              search: catalogApplicationViewCodec.apply(canonical.search, view.state),
            }
          : canonical,
        { replace: true },
      );
      return;
    }

    if (pendingApplicationIdRef.current) {
      if (pendingApplicationIdRef.current === urlApplicationId) {
        pendingApplicationIdRef.current = undefined;
      } else {
        return;
      }
    }

    if (applicationIdRef.current !== urlApplicationId) {
      setDeployDialogOpen(false);
      applicationIdRef.current = urlApplicationId;
      setMutationError(undefined);
      setApplicationId(urlApplicationId);
    }
  }, [canonical, location.pathname, navigate, urlApplicationId, urlEntry, view.state]);

  const selectionError =
    mutationError && mutationError.applicationId === applicationId
      ? mutationError.message
      : undefined;
  const selectedApplication = applicationId
    ? snapshot?.catalogApplications.find(
        (application) => application.applicationId === applicationId,
      )
    : undefined;
  const pendingKey = applicationId ? catalogPendingKey(applicationId) : undefined;
  const selectedPending = pendingKey ? pendingActions.has(pendingKey) : false;
  usePublishPanelTitles([
    ...(snapshot?.catalogApplications ?? []).map((application) =>
      panelTitle("app-catalog", "catalog", application.applicationId, application.displayName),
    ),
    ...(snapshot?.deployments ?? []).map((deployment) =>
      panelTitle(
        "apps",
        "deployment",
        deployment.applicationDeploymentId,
        applicationDeploymentPresentationName(deployment),
      ),
    ),
  ]);
  usePanelPendingNavigation(
    selectedPending,
    "An inspector operation is pending. Inspector navigation is unavailable.",
  );

  useEffect(() => {
    // Only a loaded catalog can show the application is gone; a failed first read keeps the panel.
    // The snapshot waits for every read, so a catalog read alone can't show it yet.
    if (!applicationId || !statuses.catalog.hasData || !snapshot || selectedApplication) {
      return;
    }

    queueMicrotask(() => {
      if (applicationIdRef.current === applicationId) {
        openerRef.current = headingRef.current;
        shouldRestoreHeadingFocus.current = true;
        closeSelection();
      }
    });
  }, [applicationId, closeSelection, selectedApplication, snapshot, statuses.catalog.hasData]);

  useEffect(() => {
    if (!applicationId && shouldRestoreHeadingFocus.current) {
      headingRef.current?.focus();
      shouldRestoreHeadingFocus.current = false;
    }
  }, [applicationId]);

  const updatePending = (key: string, pending: boolean) => {
    if (pending) {
      pendingActionsRef.current.add(key);
    } else {
      pendingActionsRef.current.delete(key);
    }
    setPendingActions(new Set(pendingActionsRef.current));
  };

  /**
   * Opens an accepted application deployment's panel on the Apps page as a new history entry, so
   * Back returns to the catalog inspector the deploy started from.
   */
  const openAcceptedDeployment = (applicationDeploymentId: string) => {
    // A deploy accepted after the operator left the page keeps them where they went.
    if (!mountedRef.current) return;

    panelNavigation.navigatePanel(
      {
        pathname: panelPagePath("apps"),
        // The pathname changes, so the catalog's non-panel parameters stay behind.
        search: serializePanelStack("", [
          { page: "apps", resourceId: applicationDeploymentId, screen: "deployment" },
        ]),
      },
      { bypassPendingGuard: true },
    );
  };

  const create = async (
    application: Application,
    placement: { siteNodeId: string; executionTargetId: string },
    options?: DeploymentCreationOptions,
  ) => {
    const initiatingApplicationId = application.applicationId;
    const pendingKey = catalogPendingKey(initiatingApplicationId);
    if (pendingActionsRef.current.has(pendingKey)) {
      return;
    }

    updatePending(pendingKey, true);
    try {
      const response = await client.application.createApplicationDeployment(
        options?.inferenceRouteId !== undefined
          ? {
              applicationId: application.applicationId,
              applicationDeploymentName: options.applicationDeploymentName,
              placement,
              inferenceRouteId: options.inferenceRouteId,
              expectedInferenceRouteGeneration: options.expectedInferenceRouteGeneration,
              parameterValues: [...(options.parameterValues ?? [])],
            }
          : {
              applicationId: application.applicationId,
              applicationDeploymentName:
                options?.applicationDeploymentName ?? application.displayName,
              placement,
              parameterValues: [...(options?.parameterValues ?? [])],
            },
      );
      const deployment = response.deployment;
      if (!deployment?.applicationDeploymentId) {
        if (applicationIdRef.current === initiatingApplicationId) {
          setMutationError({
            applicationId: initiatingApplicationId,
            message: "Create response protocol error: missing accepted deployment ID.",
          });
        }
        return;
      }

      applyDeploymentOverlay(deployment);

      if (applicationIdRef.current !== initiatingApplicationId) {
        return;
      }

      setDeployDialogOpen(false);
      openAcceptedDeployment(deployment.applicationDeploymentId);
    } catch (error) {
      if (applicationIdRef.current === initiatingApplicationId) {
        setMutationError({
          applicationId: initiatingApplicationId,
          message: describeMutationError("Create", error),
        });
      }
    } finally {
      updatePending(pendingKey, false);
    }
  };

  const select = (nextApplicationId: string, opener: HTMLElement) => {
    openerRef.current = opener;
    setDeployDialogOpen(false);
    pendingApplicationIdRef.current = nextApplicationId;
    applicationIdRef.current = nextApplicationId;
    setMutationError(undefined);
    setApplicationId(nextApplicationId);
    const search = selectPanel(location.search, catalogEntry(nextApplicationId));
    panelNavigation.navigatePanel(
      { pathname: panelPagePath("app-catalog"), search },
      { replace: !deepensPanelStack(location.search, search) },
    );
  };

  // A deployment the operator came from is the panel beneath, so opening it again returns there.
  const openDeployment = (entry: PanelEntry) => {
    if (panelNavigation.parent && isSamePanelEntry(panelNavigation.parent, entry)) {
      panelNavigation.back();
    } else {
      panelNavigation.navigatePanel(appendPanelLocation(location, entry));
    }
  };

  const onInspectorOpenChange = (open: boolean) => {
    if (!open && !deployDialogOpen) {
      panelNavigation.close();
    }
  };

  return (
    <div className="min-w-0">
      <div className="flex w-full flex-col gap-6">
        <PageHeader>
          <PageHeaderHeading icon={CatalogSectionIcon} ref={headingRef} tabIndex={-1}>
            Catalog
          </PageHeaderHeading>
        </PageHeader>
        {loadingFeedbackVisible && !initialLoadFailed ? (
          <p className="text-sm text-muted-foreground" role="status">
            Loading applications…
          </p>
        ) : null}

        {snapshot && (!loadingFeedbackVisible || initialLoadFailed) ? (
          <section aria-label="Catalog applications" className="flex min-w-0 flex-col gap-4">
            {statuses.catalog.error ? (
              <Notice aria-label="Catalog status" role="alert" tone="destructive">
                {statuses.catalog.error}
              </Notice>
            ) : null}
            {snapshot.catalogApplications.length ? (
              <div className="flex min-w-0 flex-col gap-2">
                <DataTableToolbar
                  facets={applicationTable.facets}
                  onViewChange={view.set}
                  ui={catalogApplicationTableUi}
                  view={view.state}
                />
                <DataTable
                  onOpenRow={({ row }, opener) => select(row.applicationId, opener)}
                  onViewChange={view.set}
                  selection={applicationId ? { rowId: applicationId } : undefined}
                  table={applicationTable}
                  ui={catalogApplicationTableUi}
                  view={view.state}
                />
              </div>
            ) : (
              <Notice role="status">No applications are available in the catalog.</Notice>
            )}
          </section>
        ) : null}

        {selectedApplication ? (
          <CatalogApplicationInspector
            application={selectedApplication}
            deployOpenerRef={deployDialogOpenerRef}
            deployments={
              statuses.deployments.hasData
                ? catalogApplicationDeployments(
                    selectedApplication.applicationId,
                    snapshot?.deployments ?? [],
                  )
                : undefined
            }
            deploymentsUnreachable={
              Boolean(statuses.deployments.error) && !statuses.deployments.hasData
            }
            error={selectionError}
            executionTargets={
              statuses.executionTargets.hasData ? snapshot?.executionTargets : undefined
            }
            key={selectedApplication.applicationId}
            onBack={() => panelNavigation.back()}
            onDeploy={() => setDeployDialogOpen(true)}
            onOpenChange={onInspectorOpenChange}
            onOpenDeployment={(applicationDeploymentId) =>
              openDeployment({
                page: "apps",
                resourceId: applicationDeploymentId,
                screen: "deployment",
              })
            }
            open={Boolean(urlApplicationId)}
            openerRef={openerRef}
            pending={selectedPending}
            suspendDismissal={deployDialogOpen}
          />
        ) : null}
        {selectedApplication && deployDialogOpen && urlApplicationId ? (
          <ApplicationDeployDialog
            application={selectedApplication}
            canCreate={
              statuses.executionTargets.hasData &&
              !statuses.executionTargets.stale &&
              !statuses.deployments.stale &&
              statuses.deployments.hasData
            }
            error={selectionError}
            executionTargetError={statuses.executionTargets.error}
            executionTargets={snapshot?.executionTargets ?? []}
            listConfiguredExecutionTargets={listConfiguredExecutionTargets}
            executionTargetsCurrent={
              statuses.executionTargets.hasData && !statuses.executionTargets.stale
            }
            onOpenChange={setDeployDialogOpen}
            openerRef={deployDialogOpenerRef}
            onSubmit={(placement, options) => void create(selectedApplication, placement, options)}
            open
            pending={selectedPending}
            routeError={routeError}
            routes={routes}
            routesCurrent={routesCurrent}
          />
        ) : null}
      </div>
    </div>
  );
}

interface MutationError {
  applicationId: string;
  message: string;
}

const noRoutes: readonly InferenceRouteView[] = [];
const routesUnavailableMessage =
  "Inference routes are unavailable. Route selection is disabled until a fresh result succeeds.";

function applicationIdFromEntry(entry: PanelEntry | undefined): string | undefined {
  return entry?.page === "app-catalog" && entry.screen === "catalog" ? entry.resourceId : undefined;
}

function catalogEntry(applicationId: string): PanelEntry {
  return { page: "app-catalog", resourceId: applicationId, screen: "catalog" };
}

function catalogPendingKey(applicationId: string): string {
  return `catalog:${applicationId}`;
}
