import { useEffect, useMemo, useRef } from "react";
import { useLocation, useNavigate } from "react-router-dom";

import { type MetricsClient, metricsClient as defaultMetricsClient } from "@/api/metrics-client";
import { type ModelClient, modelClient } from "@/api/model-client";
import { ModelsSectionIcon } from "@/app/console-section-icons";
import { useWatchedMachines } from "@/app/resource-watch";
import { DataTable } from "@/components/data-table/data-table";
import { DataTableToolbar } from "@/components/data-table/data-table-toolbar";
import { useDataTableView } from "@/components/data-table/use-data-table-view";
import { DeployActionIcon } from "@/components/ui/action-icons";
import { AddAction } from "@/components/ui/add-action";
import { EmptyState } from "@/components/ui/empty-state";
import {
  PageActions,
  PageHeader,
  PageHeaderHeading,
  PageSectionHeading,
} from "@/components/ui/page-header";
import { InferenceSection } from "@/features/inference/inference-section";
import { useInferenceMetrics } from "@/features/inference/use-inference-metrics";
import { MetricRangeControl } from "@/features/metrics/metric-range-control";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";
import { buildDataTable } from "@/lib/data-table-model";
import { usePanelNavigation } from "@/panels/panel-navigation";
import {
  activePanel,
  canonicalizePanelLocation,
  panelPagePath,
  selectPanelLocation,
} from "@/panels/panel-stack";

import { DeploymentSidebar } from "./deployment-sidebar";
import {
  modelDeploymentRows,
  modelDeploymentTableDefinition,
  modelDeploymentTableUi,
  modelDeploymentViewCodec,
} from "./model-deployment-table";
import { useSharedModelsPolling } from "./models-polling-context";

export interface ModelsPageProps {
  client?: ModelClient;
  metricsClient?: MetricsClient;
}

/**
 * The models page: the site's inference over the page range, by model deployment, then the
 * running model deployments, each opening its sidebar on the page's range.
 */
export function ModelsPage({
  client = modelClient,
  metricsClient = defaultMetricsClient,
}: ModelsPageProps) {
  const location = useLocation();
  const navigate = useNavigate();
  const panelNavigation = usePanelNavigation();
  const canonical = useMemo(
    () => canonicalizePanelLocation({ pathname: location.pathname, search: location.search }),
    [location.pathname, location.search],
  );
  const urlEntry = activePanel(canonical.search);
  const selectedDeploymentId = urlEntry?.page === "models" ? urlEntry.resourceId : undefined;
  const { error: pollingError, snapshot } = useSharedModelsPolling();
  const loadingFeedbackVisible = useDelayedLoading(!snapshot);
  const metrics = useInferenceMetrics(metricsClient, "deployments");
  const machines = useWatchedMachines()?.value?.machines;
  const runningModels = snapshot?.runningModels ?? noDeployments;
  const view = useDataTableView(modelDeploymentViewCodec);
  const deploymentRows = modelDeploymentRows(runningModels, {
    machines,
    trafficOf: metrics.rowValues,
  });
  // Rows read the traffic of the latest metrics, so they are built on every render.
  const deploymentTable = buildDataTable(
    modelDeploymentTableDefinition,
    deploymentRows,
    view.state,
  );
  const openerRef = useRef<HTMLElement | null>(null);
  const headingRef = useRef<HTMLHeadingElement>(null);

  useEffect(() => {
    if (canonical.recovered || (urlEntry && !selectedDeploymentId)) {
      void navigate(canonical, { replace: true });
    }
  }, [canonical, navigate, selectedDeploymentId, urlEntry]);

  const select = (modelDeploymentId: string, opener: HTMLElement) => {
    openerRef.current = opener;
    panelNavigation.navigatePanel(
      selectPanelLocation(location, {
        page: "models",
        resourceId: modelDeploymentId,
        screen: "deployment",
      }),
    );
  };

  return (
    <div className="min-w-0">
      <div className="flex w-full flex-col gap-6">
        <PageHeader>
          <PageHeaderHeading icon={ModelsSectionIcon} ref={headingRef} tabIndex={-1}>
            Models
          </PageHeaderHeading>
          <PageActions>
            <MetricRangeControl label="Models range" metrics={metrics} />
            <AddAction icon={DeployActionIcon} label="Deploy" to={panelPagePath("model-catalog")} />
          </PageActions>
        </PageHeader>

        <InferenceSection metrics={metrics} />

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
          <section aria-labelledby="running-models-heading" className="flex flex-col gap-4">
            <div>
              <PageSectionHeading id="running-models-heading">Running</PageSectionHeading>
            </div>
            {snapshot.runningModels.length > 0 ? (
              <div className="flex min-w-0 flex-col gap-2">
                <DataTableToolbar
                  facets={deploymentTable.facets}
                  onViewChange={view.set}
                  ui={modelDeploymentTableUi}
                  view={view.state}
                />
                <DataTable
                  onOpenRow={({ row }, opener) => select(row.deploymentId, opener)}
                  onViewChange={view.set}
                  selection={selectedDeploymentId ? { rowId: selectedDeploymentId } : undefined}
                  table={deploymentTable}
                  ui={modelDeploymentTableUi}
                  view={view.state}
                />
              </div>
            ) : (
              <EmptyState
                action={
                  <AddAction
                    icon={DeployActionIcon}
                    label="Deploy"
                    to={panelPagePath("model-catalog")}
                  />
                }
                description="Deploy a model from the catalog to start serving it on one of your machines."
                icon={ModelsSectionIcon}
                title="No models running"
              />
            )}
          </section>
        ) : null}

        <DeploymentSidebar
          client={client}
          entry={urlEntry}
          fallbackFocusRef={headingRef}
          initialRange={metrics.range?.label}
          metricsClient={metricsClient}
          openerRef={openerRef}
        />
      </div>
    </div>
  );
}

const noDeployments: readonly ModelDeployment[] = [];
