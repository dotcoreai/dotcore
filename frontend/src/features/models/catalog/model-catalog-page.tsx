import { create } from "@bufbuild/protobuf";
import { useEffect, useRef, useState } from "react";
import { useLocation } from "react-router-dom";

import { useClients } from "@/api/clients";
import { DataTable } from "@/components/data-table/data-table";
import { DataTableToolbar } from "@/components/data-table/data-table-toolbar";
import { Button } from "@/components/ui/button";
import { Notice } from "@/components/ui/notice";
import { PageActions, PageHeader, PageHeaderHeading } from "@/components/ui/page-header";
import { CatalogSectionIcon } from "@/components/ui/section-icons";
import { modelDeploymentPresentationName } from "@/domain/model-projection";
import { ModelDeployDialog } from "@/features/models/deploy/model-deploy-dialog";
import { CatalogModelInspector } from "@/features/models/inspector/model-inspector";
import { useSharedModelsPolling } from "@/features/models/polling/models-polling-context";
import { catalogModelDeployments } from "@/features/models/profiles/model-row";
import { sameSizingScenario } from "@/features/models/profiles/sizing-scenario";
import { UpstreamDeployDialog } from "@/features/models/upstream/upstream-model";
import { type EngineRelease } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import { type HardwareClass } from "@/gen/dotcore/catalog/v1/hardware_class_pb";
import type { UpstreamModelTemplate } from "@/gen/dotcore/catalog/v1/upstream_model_template_pb";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";
import { type ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import { LocalModelDeploymentConfigurationSchema } from "@/gen/dotcore/model/v1/service_pb";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";
import { usePanelNavigation } from "@/panels/panel-navigation";
import { usePanelPendingNavigation } from "@/panels/panel-navigation-pending";
import { appendPanel, panelPagePath } from "@/panels/panel-stack";
import { panelTitle } from "@/panels/panel-titles";
import { usePublishPanelTitles } from "@/panels/panel-titles-context";
import {
  useWatchedApplicationDeployments,
  useWatchedRoutes,
} from "@/resource-watch/resource-watch";

import { catalogViewCodec } from "./catalog-view-state";
import { ConnectUpstreamModelAction } from "./connect-upstream-model-action";
import { modelDeployRequests, useCatalogDeploy } from "./use-catalog-deploy";
import { useCatalogPanelSizing } from "./use-catalog-panel-sizing";
import {
  type CatalogSelection,
  isSameSelection,
  useCatalogSelection,
} from "./use-catalog-selection";
import { useCatalogView } from "./use-catalog-view";

/**
 * The model Catalog page: the Catalog table with its toolbar, the "Connect upstream" action,
 * and their inspectors. The table view lives in the URL. It owns every deploy; an accepted
 * deployment opens its panel on the Models page.
 */
export function ModelCatalogPage() {
  const { modelClient: client, siteClient } = useClients();
  const location = useLocation();
  const panelNavigation = usePanelNavigation();
  const [upstreamTemplates, setUpstreamTemplates] = useState<UpstreamModelTemplate[]>();
  const [engineReleases, setEngineReleases] = useState<EngineRelease[]>();
  const [hardwareClasses, setHardwareClasses] = useState<HardwareClass[]>();
  const { applyDeploymentOverlay, error: pollingError, snapshot } = useSharedModelsPolling();
  const loadingFeedbackVisible = useDelayedLoading(!snapshot);
  const selectionRef = useRef<CatalogSelection | undefined>(undefined);
  const deploy = useCatalogDeploy({
    applyDeploymentOverlay,
    client,
    panelNavigation,
    selectionRef,
  });
  const { deployModel, deployPending, deployTemplate } = deploy;
  const siteReadsActive = !deployModel;
  const catalog = useCatalogView({
    client,
    engineReleases,
    hardwareClasses,
    siteClient,
    siteReadsActive,
    snapshot,
  });
  const { catalogRunsOnFilter, catalogTable, catalogUi, view } = catalog;
  const {
    headingRef,
    onInspectorOpenChange,
    openDeployment,
    openerRef,
    select,
    selectedModel,
    selection,
    urlSelection,
  } = useCatalogSelection({
    applyView: catalog.applyView,
    panelNavigation,
    selectionRef,
    setDeployError: deploy.setDeployError,
    setDeployModel: deploy.setDeployModel,
    setDeployProfileId: deploy.setDeployProfileId,
    setUpstreamError: deploy.setUpstreamError,
    snapshot,
    viewState: view.state,
  });
  const deployDialogOpenerRef = useRef<HTMLButtonElement>(null);

  const noneRunEmpty =
    catalogRunsOnFilter && catalogTable.noneRun && !catalogTable.rows.length ? (
      <div className="flex flex-col items-center gap-1">
        <p>No model matches the filter.</p>
        <Button onClick={catalog.removeRunsOn} size="xs" type="button" variant="link">
          Show all models
        </Button>
      </div>
    ) : undefined;

  useEffect(() => {
    void client.catalog
      .listHardwareClasses({})
      .then((response) => setHardwareClasses(response.hardwareClasses))
      .catch(() => setHardwareClasses([]));
  }, [client]);

  useEffect(() => {
    void client.catalog
      .listEngineReleases({})
      .then((response) => setEngineReleases(response.engineReleases))
      .catch(() => setEngineReleases([]));
  }, [client]);

  useEffect(() => {
    let active = true;
    void client.catalog
      .listUpstreamModelTemplates({})
      .then((response) => {
        if (active) setUpstreamTemplates(response.upstreamModelTemplates);
      })
      .catch(() => {
        if (active) setUpstreamTemplates([]);
      });
    return () => {
      active = false;
    };
  }, [client]);

  const routeViews = useWatchedRoutes()?.value?.routes ?? noRouteViews;
  // Application deployments name their memory in the deploy dialog's preview, so they are watched
  // only while it is open.
  const applications = useWatchedApplicationDeployments({ enabled: Boolean(deployModel) })?.value
    ?.deployments;

  const selectionError =
    deploy.deployError && isSameSelection(deploy.deployError.selection, selection)
      ? deploy.deployError.message
      : undefined;
  const panelModelId =
    selection?.kind === "catalog" || selection?.kind === "catalog-profile"
      ? selection.modelId
      : undefined;
  const panel = useCatalogPanelSizing({
    basis: catalog.basis,
    catalogLocationScope: catalog.catalogLocationScope,
    engineReleases,
    hardwareClasses,
    listProfileCompatibility: catalog.listProfileCompatibility,
    locationSearch: location.search,
    memoryPending: catalog.memoryPending,
    panelModelId,
    scenario: catalog.scenario,
    selectedModel,
    siteCompatibility: catalog.siteCompatibility,
    siteReadsActive,
    siteTargets: catalog.siteTargets,
    snapshot,
  });
  usePublishPanelTitles([
    ...(snapshot?.availableModels ?? []).map((model) =>
      panelTitle("model-catalog", "catalog", model.modelId, model.displayName),
    ),
    ...(snapshot?.runningModels ?? []).map((deployment) =>
      panelTitle(
        "models",
        "deployment",
        deployment.modelDeploymentId,
        modelDeploymentPresentationName(deployment),
      ),
    ),
  ]);
  usePanelPendingNavigation(
    selection?.kind === "catalog" || selection?.kind === "catalog-profile"
      ? deployPending(selection.modelId)
      : false,
    "An inspector operation is pending. Inspector navigation is unavailable.",
  );

  return (
    <div className="min-w-0">
      <div className="flex w-full flex-col gap-6">
        <PageHeader>
          <PageHeaderHeading icon={CatalogSectionIcon} ref={headingRef} tabIndex={-1}>
            Catalog
          </PageHeaderHeading>
          {upstreamTemplates?.length ? (
            <PageActions>
              <ConnectUpstreamModelAction
                onSelect={deploy.selectUpstreamTemplate}
                templates={upstreamTemplates}
              />
            </PageActions>
          ) : null}
        </PageHeader>

        {loadingFeedbackVisible && !pollingError ? (
          <p className="text-sm text-muted-foreground" role="status">
            Loading models…
          </p>
        ) : null}

        {pollingError ? (
          <p className="text-sm text-destructive" role="alert">
            {pollingError}
          </p>
        ) : null}

        {snapshot && !loadingFeedbackVisible ? (
          snapshot.availableModels.length > 0 ? (
            <section aria-label="Catalog models" className="flex min-w-0 flex-col gap-2">
              <DataTableToolbar
                chipSettings={catalog.chipSettings}
                facets={catalogTable.facets}
                onReset={catalog.resetView}
                onViewChange={view.set}
                ui={catalogUi}
                view={view.state}
              />
              {catalog.runsOnNotice ? (
                <p className="text-xs text-muted-foreground" role="status">
                  {catalog.runsOnNotice}
                </p>
              ) : null}
              {catalog.sizingNotice ? (
                <p className="text-xs text-muted-foreground" role="status">
                  {catalog.sizingNotice}
                </p>
              ) : null}
              <DataTable
                empty={noneRunEmpty}
                footnote={catalog.hiddenNote}
                onOpenChild={({ row }, profile, opener) =>
                  select(
                    { kind: "catalog-profile", modelId: row.modelId, profileId: profile.profileId },
                    opener,
                  )
                }
                onOpenRow={({ row }, opener) =>
                  select({ kind: "catalog", modelId: row.modelId }, opener)
                }
                onViewChange={view.set}
                selection={
                  selection?.kind === "catalog"
                    ? { rowId: selection.modelId }
                    : selection?.kind === "catalog-profile"
                      ? { childId: selection.profileId, rowId: selection.modelId }
                      : undefined
                }
                table={catalogTable}
                ui={catalogUi}
                view={view.state}
              />
            </section>
          ) : (
            <Notice role="status">No models are available to deploy.</Notice>
          )
        ) : null}

        {(selection?.kind === "catalog" || selection?.kind === "catalog-profile") &&
        selectedModel ? (
          <CatalogModelInspector
            catalogChips={panel.panelChips}
            compatibility={panel.catalogCompatibility}
            deployOpenerRef={deployDialogOpenerRef}
            deployments={catalogModelDeployments(
              selectedModel.modelId,
              snapshot?.runningModels ?? [],
            )}
            error={selectionError}
            executionTargets={catalog.siteTargets}
            memoryPending={panel.panelMemoryPending}
            model={selectedModel}
            onBack={() => panelNavigation.back()}
            onDeploy={() =>
              deploy.startDeploy(
                selectedModel,
                selection.kind === "catalog-profile" ? selection.profileId : undefined,
                panel.panelChips ?? catalogViewCodec.parse(location.search).predicates,
                panel.panelScenario,
                panel.panelBasis,
              )
            }
            onOpenChange={onInspectorOpenChange}
            onOpenDeployment={(modelDeploymentId) =>
              openDeployment({
                page: "models",
                resourceId: modelDeploymentId,
                screen: "deployment",
              })
            }
            onSelectProfile={(profileId) =>
              panelNavigation.navigatePanel({
                pathname: panelPagePath("model-catalog"),
                search: appendPanel(location.search, {
                  page: "model-catalog",
                  profileId,
                  resourceId: selectedModel.modelId,
                  screen: "catalog-profile",
                }),
              })
            }
            open={Boolean(urlSelection)}
            openerRef={openerRef}
            pending={deployPending(selectedModel.modelId)}
            profileId={selection.kind === "catalog-profile" ? selection.profileId : undefined}
            row={
              panel.panelRow ??
              catalog.modelRows.find((row) => row.modelId === selectedModel.modelId)!
            }
            screen={selection.kind === "catalog-profile" ? "profile-detail" : "overview"}
            suspendDismissal={Boolean(deployModel)}
          />
        ) : null}

        {deployModel ? (
          <ModelDeployDialog
            {...modelDeployRequests(client, deployModel)}
            applications={applications}
            error={selectionError}
            existingRouteNames={routeViews.flatMap((view) => (view.route ? [view.route.name] : []))}
            initialProfileId={deploy.deployProfileId}
            catalogChips={deploy.deployChips}
            deployments={snapshot?.runningModels ?? noDeployments}
            model={deployModel}
            profileEstimates={
              sameSizingScenario(deploy.deployScenario, catalog.scenario)
                ? catalog.profileEstimates
                : undefined
            }
            memoryBasis={deploy.deployBasis}
            scenario={deploy.deployScenario}
            onOpenChange={(open) => {
              if (!open) deploy.closeDeployDialog(deployModel);
            }}
            routeConflict={deploy.routeConflict}
            engineReleases={engineReleases ?? []}
            hardwareClasses={hardwareClasses ?? []}
            onSubmit={({ configuration, exposure, name, placement, profileId }) => {
              void deploy.load(
                deployModel,
                profileId,
                create(LocalModelDeploymentConfigurationSchema, configuration),
                placement,
                exposure,
                name,
              );
            }}
            listConfiguredExecutionTargets={catalog.listConfiguredExecutionTargets}
            listProfileCompatibility={catalog.listProfileCompatibility}
            openerRef={deployDialogOpenerRef}
            open
            pending={deployPending(deployModel.modelId)}
          />
        ) : null}
        {deployTemplate ? (
          <UpstreamDeployDialog
            open
            template={deployTemplate}
            error={deploy.upstreamError}
            existingRouteNames={routeViews.flatMap((view) => (view.route ? [view.route.name] : []))}
            onOpenChange={(open) => {
              if (!open) deploy.closeUpstreamDialog();
            }}
            onSubmit={(values) => {
              void deploy.deployUpstream(deployTemplate, values);
            }}
            pending={deploy.upstreamDeployPending}
            routeConflict={deploy.routeConflict}
          />
        ) : null}
      </div>
    </div>
  );
}

const noRouteViews: readonly InferenceRouteView[] = [];

const noDeployments: readonly ModelDeployment[] = [];
