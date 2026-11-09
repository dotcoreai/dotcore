import { useEffect, useMemo, useRef } from "react";
import { useLocation, useNavigate } from "react-router-dom";

import { type MetricsClient, metricsClient as defaultMetricsClient } from "@/api/metrics-client";
import { type RouteClient } from "@/api/route-client";
import { RoutesSectionIcon } from "@/app/console-section-icons";
import { DataTable } from "@/components/data-table/data-table";
import { DataTableToolbar } from "@/components/data-table/data-table-toolbar";
import { useDataTableView } from "@/components/data-table/use-data-table-view";
import { AddAction } from "@/components/ui/add-action";
import { EmptyState } from "@/components/ui/empty-state";
import { Notice } from "@/components/ui/notice";
import {
  PageActions,
  PageHeader,
  PageHeaderHeading,
  PageSectionHeading,
} from "@/components/ui/page-header";
import { InferenceSection } from "@/features/inference/inference-section";
import { useInferenceMetrics } from "@/features/inference/use-inference-metrics";
import { MetricRangeControl } from "@/features/metrics/metric-range-control";
import { buildDataTable } from "@/lib/data-table-model";
import { canonicalizePanelLocation } from "@/panels/panel-stack";

import { routeRows, routeTableDefinition, routeTableUi, routeViewCodec } from "./route-table";
import { useRouteSidebarHost } from "./use-route-sidebar-host";

export interface RoutesPageProps {
  client?: RouteClient;
  metricsClient?: MetricsClient;
}

/**
 * The routes page: the site's inference over the page range, by route, then the routes as a table
 * with their traffic over the same range, each row opening its sidebar on the page's range.
 */
export function RoutesPage({ client, metricsClient = defaultMetricsClient }: RoutesPageProps) {
  const location = useLocation();
  const navigate = useNavigate();
  const headingRef = useRef<HTMLHeadingElement>(null);
  const openerRef = useRef<HTMLElement>(null);
  const metrics = useInferenceMetrics(metricsClient, "routes");
  const host = useRouteSidebarHost({
    client,
    initialRange: metrics.range?.label,
    metricsClient,
    openerRef,
    returnFocusRef: headingRef,
  });
  const view = useDataTableView(routeViewCodec);
  const table = buildDataTable(
    routeTableDefinition,
    routeRows(host.routes ?? [], host.deploymentMentions, metrics.rowValues),
    view.state,
  );
  const canonical = useMemo(
    () => canonicalizePanelLocation({ pathname: location.pathname, search: location.search }),
    [location.pathname, location.search],
  );

  useEffect(() => {
    if (!canonical.recovered) return;
    void navigate(canonical, { replace: true });
  }, [canonical, navigate]);

  return (
    <div className="min-w-0">
      <div className="flex w-full flex-col gap-6">
        <PageHeader>
          <PageHeaderHeading icon={RoutesSectionIcon} ref={headingRef} tabIndex={-1}>
            Routes
          </PageHeaderHeading>
          <PageActions>
            <MetricRangeControl label="Routes range" metrics={metrics} />
            <AddAction label="Create route" onClick={host.openCreate} />
          </PageActions>
        </PageHeader>
        <InferenceSection metrics={metrics} />
        {host.loadError ? (
          <Notice role="alert" tone="destructive">
            {host.loadError}
          </Notice>
        ) : null}
        {host.error ? (
          <Notice role="alert" tone="destructive">
            {host.error}
          </Notice>
        ) : null}
        <section aria-labelledby="inference-routes-heading" className="flex flex-col gap-4">
          <div>
            <PageSectionHeading id="inference-routes-heading">Routes</PageSectionHeading>
          </div>
          {host.loadingVisible && !host.loadError ? (
            <p className="text-sm text-muted-foreground" role="status">
              Loading routes…
            </p>
          ) : !host.routes ? null : host.routes.length ? (
            <div className="flex min-w-0 flex-col gap-2">
              <DataTableToolbar
                facets={table.facets}
                onViewChange={view.set}
                ui={routeTableUi}
                view={view.state}
              />
              <DataTable
                onOpenRow={({ row }, opener) => {
                  const selected = host.routes?.find(
                    (candidate) => candidate.route?.inferenceRouteId === row.routeId,
                  );
                  if (selected) host.select(selected, opener);
                }}
                onViewChange={view.set}
                selection={host.selectedRouteId ? { rowId: host.selectedRouteId } : undefined}
                table={table}
                ui={routeTableUi}
                view={view.state}
              />
            </div>
          ) : (
            <EmptyState
              action={<AddAction label="Create route" onClick={host.openCreate} />}
              description="Create a route to give one or more model deployments a single name that clients can call."
              icon={RoutesSectionIcon}
              title="No routes yet"
            />
          )}
        </section>
        {host.sidebar}
      </div>
    </div>
  );
}
